//! J13 — real-time watcher for the discovery daemon.
//!
//! The daemon *polls*: every `poll_interval_secs` it runs a discovery cycle.
//! `pilot daemon --watch` keeps that poll (nothing local signals a new GitHub
//! issue or a red CI run) but also wakes early on two kinds of event, each
//! debounced into one wake:
//!
//! - **A file changes** in the working tree (not `.git/`, not the daemon's own
//!   `.wingman/` state, not anything `.gitignore` excludes). That runs a cycle
//!   of the local sources only — `todos`, `coverage_gaps`, `intake`, `ask` —
//!   so saving a file never costs a `gh` call.
//! - **A git hook fires** (`post-commit`, `post-merge`, `post-checkout`,
//!   `post-rewrite`). That runs a full cycle, the same as a poll.
//!
//! Either way the candidates go through the unchanged `daemon::run_cycle`
//! → score → trust → queue → `max_auto_dispatch_per_cycle` path. The watcher
//! only decides *when* a cycle runs and *which* sources it asks.
//!
//! The hooks are not shell scripts. Each is a file whose `#!` line names the
//! wingman binary itself, so git runs `wingman <hook-file> <args>` directly:
//! the kernel does that on Linux and macOS, and Git for Windows does it by
//! looking the interpreter's file name up on `PATH`. [`hook_invocation`]
//! recognises that argv and [`record_hook_event`] drops a signal file under
//! `.wingman/watch/`, which the watcher sees like any other file change.
//! Nothing talks to a socket, and a hook that fires with no daemon running
//! just rewrites one small file. A hook records nothing in a repo that has no
//! `.wingman/watch/` (made by `pilot hooks install` or a watching daemon), so
//! hooks installed into a `core.hooksPath` shared by other repos leave those
//! repos alone.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};
use tokio::sync::mpsc;

use crate::pr::CommandRunner;

/// The hooks `pilot hooks install` writes. All run *after* git has done its
/// work and git ignores their exit status, so a broken or missing wingman can
/// never block a commit, merge or checkout. No `pre-*` hook is ever installed.
pub const HOOK_NAMES: &[&str] = &["post-commit", "post-merge", "post-checkout", "post-rewrite"];

/// Second line of every hook this module writes. Recognising a hook invocation
/// and uninstalling both require it, so a user's own hook is never mistaken
/// for ours.
const HOOK_MARKER: &str = "# wingman-pilot-watch-hook";

/// Where hook signals land, relative to the repo root.
const SIGNAL_DIR: &str = ".wingman/watch";

/// Sources a file change may run. Everything else (the `gh`-backed ones) waits
/// for a poll or a git event.
pub const LOCAL_SOURCES: &[&str] = &["todos", "coverage_gaps", "intake", "ask"];

/// Why the daemon's wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// `poll_interval_secs` elapsed.
    Poll,
    /// An installed git hook fired.
    GitEvent,
    /// A relevant file in the working tree changed.
    FileChange,
}

/// The daemon config a cycle woken by `wake` should run: every configured
/// source for a poll or git event, only [`LOCAL_SOURCES`] for a file change.
pub fn cycle_config(
    cfg: &wingman_config::PilotDaemonConfig,
    wake: Wake,
) -> wingman_config::PilotDaemonConfig {
    let mut cfg = cfg.clone();
    if wake == Wake::FileChange {
        cfg.sources.retain(|s| LOCAL_SOURCES.contains(&s.as_str()));
    }
    cfg
}

/// A live recursive watch on a repo root. Dropping it stops watching.
pub struct Watcher {
    root: PathBuf,
    /// The root as the OS reports it. macOS reports `/private/var/...` for a
    /// watch on `/var/...`, so event paths are matched against both.
    canonical_root: PathBuf,
    intake: Option<PathBuf>,
    rx: mpsc::UnboundedReceiver<Vec<PathBuf>>,
    _debouncer: Debouncer<RecommendedWatcher>,
}

impl Watcher {
    /// Watch `root` recursively, collapsing each burst of events into one
    /// batch after `debounce` of quiet. `intake` is the intake directory when
    /// that source is enabled; changes there count even if it is gitignored.
    pub fn start(root: &Path, intake: Option<PathBuf>, debounce: Duration) -> Result<Self, String> {
        // Create the directories events are expected in before watching. On
        // Linux a file written into a directory created after the watch began
        // can land before notify adds a watch for that directory, and the very
        // first hook signal would be lost.
        let signal_dir = root.join(SIGNAL_DIR);
        std::fs::create_dir_all(&signal_dir)
            .map_err(|e| format!("creating {}: {e}", signal_dir.display()))?;
        if let Some(dir) = &intake {
            std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }

        let (tx, rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();
        let mut debouncer = new_debouncer(debounce, move |res: DebounceEventResult| match res {
            Ok(events) => {
                let paths: Vec<PathBuf> = events.into_iter().map(|e| e.path).collect();
                if !paths.is_empty() {
                    let _ = tx.send(paths);
                }
            }
            Err(e) => tracing::warn!("pilot watch error: {e:?}"),
        })
        .map_err(|e| format!("notify: {e}"))?;
        // ponytail: one recursive watch over the whole root, ignored trees
        // (target/, node_modules/) included, filtered after the fact. On Linux
        // that is one inotify watch per directory; watch only non-ignored
        // directories if a large repo hits fs.inotify.max_user_watches.
        debouncer
            .watcher()
            .watch(root, RecursiveMode::Recursive)
            .map_err(|e| format!("watching {}: {e}", root.display()))?;

        Ok(Self {
            root: root.to_path_buf(),
            canonical_root: std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
            intake,
            rx,
            _debouncer: debouncer,
        })
    }

    /// Wait until `deadline` ([`Wake::Poll`]) or until a batch of events
    /// [`classify`] as relevant, whichever comes first. Batches that are all
    /// noise (build output, `.git/` internals, the daemon's own queue writes)
    /// are swallowed without waking the daemon.
    pub async fn wait(
        &mut self,
        runner: &dyn CommandRunner,
        deadline: tokio::time::Instant,
    ) -> Wake {
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return Wake::Poll,
                batch = self.rx.recv() => {
                    let Some(paths) = batch else {
                        // The debouncer is owned by `self`, so this cannot
                        // happen; if it somehow does, degrade to polling.
                        tokio::time::sleep_until(deadline).await;
                        return Wake::Poll;
                    };
                    let rel: Vec<PathBuf> = paths
                        .iter()
                        .filter_map(|p| {
                            p.strip_prefix(&self.root)
                                .or_else(|_| p.strip_prefix(&self.canonical_root))
                                .ok()
                                .map(Path::to_path_buf)
                        })
                        .collect();
                    let intake = self.intake.as_ref().and_then(|d| {
                        d.strip_prefix(&self.root).ok().map(Path::to_path_buf)
                    });
                    if let Some(wake) = classify(runner, &self.root, intake.as_deref(), &rel) {
                        return wake;
                    }
                }
            }
        }
    }
}

/// Decide whether one debounced batch of changed paths (relative to `root`)
/// should wake the daemon. A hook signal wins over a file change, since it
/// runs the wider cycle.
///
/// Dropped without waking: anything under `.git/`, anything under `.wingman/`
/// except a hook signal or the intake directory (the daemon writes its queue
/// and runs there, and must not wake itself), and anything
/// `git check-ignore` excludes.
pub fn classify(
    runner: &dyn CommandRunner,
    root: &Path,
    intake: Option<&Path>,
    paths: &[PathBuf],
) -> Option<Wake> {
    let signal_dir = Path::new(SIGNAL_DIR);
    let mut files = false;
    let mut to_check = Vec::new();
    for p in paths {
        if p.parent() == Some(signal_dir) {
            return Some(Wake::GitEvent);
        }
        if intake.is_some_and(|d| p.starts_with(d) && p != d) {
            files = true;
            continue;
        }
        match p.components().next() {
            Some(c) if c.as_os_str() == ".git" || c.as_os_str() == ".wingman" => continue,
            None => continue,
            _ => to_check.push(p),
        }
    }
    if files || any_not_ignored(runner, root, &to_check) {
        Some(Wake::FileChange)
    } else {
        None
    }
}

/// Whether any of `paths` is not excluded by `.gitignore`. Fails open: if
/// `git check-ignore` can't answer (not a repo, no git), every path counts.
fn any_not_ignored(runner: &dyn CommandRunner, root: &Path, paths: &[&PathBuf]) -> bool {
    // Chunked so a batch with a relevant path near the front stops asking git
    // early instead of sending a whole build's worth of paths.
    // ponytail: a build flooding target/ costs one git process per 256 paths
    // per debounce window; skip ignored directories up front if that shows.
    for chunk in paths.chunks(256) {
        let rel: Vec<String> = chunk
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        let input = rel.join("\n") + "\n";
        let out = match runner.run_with_stdin(
            "git",
            &["check-ignore", "--stdin"],
            root,
            input.as_bytes(),
        ) {
            Ok(out) => out,
            Err(_) => return true,
        };
        match out.status {
            // Some were ignored: they are the lines echoed back.
            Some(0) => {
                let ignored: HashSet<&str> = out.stdout.lines().map(str::trim).collect();
                if rel.iter().any(|p| !ignored.contains(p.as_str())) {
                    return true;
                }
            }
            // None were ignored.
            Some(1) => return true,
            // Fatal (128: not a git repo) or killed.
            _ => return true,
        }
    }
    false
}

/// The hooks directory git will actually run hooks from, honouring
/// `core.hooksPath` and linked worktrees.
pub fn hooks_dir(runner: &dyn CommandRunner, repo_root: &Path) -> Result<PathBuf, String> {
    let out = runner
        .run("git", &["rev-parse", "--git-path", "hooks"], repo_root)
        .map_err(|e| format!("git rev-parse failed: {e}"))?;
    if !out.success() {
        return Err(format!("not a git repository: {}", out.stderr.trim()));
    }
    let dir = PathBuf::from(out.stdout.trim());
    Ok(if dir.is_absolute() {
        dir
    } else {
        repo_root.join(dir)
    })
}

/// The body of an installed hook: a `#!` line naming `exe`, then the marker.
///
/// Refuses a path the `#!` line cannot carry. On Linux and macOS the kernel
/// splits the line at whitespace and caps its length; Git for Windows uses
/// only the file name after the last slash (resolved on `PATH`), so a space in
/// the directory is fine there.
pub fn hook_script(exe: &Path) -> Result<String, String> {
    let path = exe.to_string_lossy().replace('\\', "/");
    if path.contains(['\n', '\r']) {
        return Err(format!("{path:?} cannot appear in a #! line"));
    }
    if !cfg!(windows) && (path.contains(char::is_whitespace) || path.len() > 250) {
        return Err(format!(
            "{path:?} cannot appear in a #! line (whitespace, or longer than 250 bytes); \
             install wingman somewhere with a shorter, space-free path"
        ));
    }
    Ok(format!(
        "#!{path}\n{HOOK_MARKER}\n\
         # Git runs this file with the wingman binary above; it records the event\n\
         # for `wingman pilot daemon --watch`. Remove with `wingman pilot hooks uninstall`.\n"
    ))
}

fn is_wingman_hook(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|s| s.lines().nth(1) == Some(HOOK_MARKER))
}

/// Result of [`install_hooks`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HookInstall {
    pub installed: Vec<PathBuf>,
    /// Hooks left alone because a hook that isn't ours already exists there.
    pub skipped: Vec<PathBuf>,
}

/// Write every hook in [`HOOK_NAMES`] into the repo's hooks directory,
/// pointing at `exe`. A hook of ours is rewritten (so a moved binary can be
/// re-pointed); anyone else's is never touched.
pub fn install_hooks(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    exe: &Path,
) -> Result<HookInstall, String> {
    let script = hook_script(exe)?;
    let dir = hooks_dir(runner, repo_root)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    // Marks this repo as one the hooks should record for (see
    // `record_hook_event`).
    let signal_dir = repo_root.join(SIGNAL_DIR);
    std::fs::create_dir_all(&signal_dir)
        .map_err(|e| format!("creating {}: {e}", signal_dir.display()))?;
    let mut result = HookInstall::default();
    for name in HOOK_NAMES {
        let path = dir.join(name);
        if path.exists() && !is_wingman_hook(&path) {
            result.skipped.push(path);
            continue;
        }
        std::fs::write(&path, &script).map_err(|e| format!("writing {}: {e}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("chmod {}: {e}", path.display()))?;
        }
        result.installed.push(path);
    }
    Ok(result)
}

/// Remove the hooks [`install_hooks`] wrote, and nothing else. Returns the
/// paths removed.
pub fn uninstall_hooks(
    runner: &dyn CommandRunner,
    repo_root: &Path,
) -> Result<Vec<PathBuf>, String> {
    let dir = hooks_dir(runner, repo_root)?;
    let mut removed = Vec::new();
    for name in HOOK_NAMES {
        let path = dir.join(name);
        if is_wingman_hook(&path) {
            std::fs::remove_file(&path).map_err(|e| format!("removing {}: {e}", path.display()))?;
            removed.push(path);
        }
    }
    Ok(removed)
}

/// If this process is git running one of our hooks — `argv[1]` is a hook file
/// named in [`HOOK_NAMES`] that carries the marker — the hook's name.
pub fn hook_invocation(args: &[OsString]) -> Option<&'static str> {
    let path = Path::new(args.get(1)?);
    let file_name = path.file_name()?.to_str()?;
    let name = HOOK_NAMES.iter().find(|n| **n == file_name)?;
    is_wingman_hook(path).then_some(*name)
}

/// Record that `hook` fired, for a watcher on `worktree_root`. Git runs hooks
/// from the top of the working tree, so that is the process's cwd.
///
/// Does nothing unless `worktree_root/.git` is a directory: a linked worktree
/// (where `.git` is a file) shares the main repo's hooks, and pilot creates
/// one per task — its commits and checkouts are the daemon's own work, not
/// events to react to, and a signal file written there would end up in the
/// task's diff.
///
/// Nor unless `worktree_root/.wingman/watch/` already exists. A
/// `core.hooksPath` can be shared by every repo on the machine (a global
/// setting), and a hook installed there must not create `.wingman/` in each
/// repo that happens to commit.
pub fn record_hook_event(worktree_root: &Path, hook: &str) -> std::io::Result<()> {
    let dir = worktree_root.join(SIGNAL_DIR);
    if !worktree_root.join(".git").is_dir() || !dir.is_dir() {
        return Ok(());
    }
    // A changing body, so the write is seen as a modification on every
    // platform even when the file already exists.
    std::fs::write(
        dir.join(hook),
        format!("{}\n", chrono::Utc::now().to_rfc3339()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::CommandOut;
    use std::sync::Mutex;

    /// Answers `git check-ignore --stdin` by echoing back the inputs that
    /// start with `target/`, and `git rev-parse --git-path hooks` with
    /// `.git/hooks`. Records every stdin it was given.
    #[derive(Default)]
    struct FakeGit {
        stdins: Mutex<Vec<String>>,
    }
    impl CommandRunner for FakeGit {
        fn run(&self, _p: &str, args: &[&str], _cwd: &Path) -> std::io::Result<CommandOut> {
            assert_eq!(args, ["rev-parse", "--git-path", "hooks"]);
            Ok(CommandOut {
                status: Some(0),
                stdout: ".git/hooks\n".into(),
                stderr: String::new(),
            })
        }
        fn run_with_stdin(
            &self,
            _p: &str,
            args: &[&str],
            _cwd: &Path,
            stdin: &[u8],
        ) -> std::io::Result<CommandOut> {
            assert_eq!(args, ["check-ignore", "--stdin"]);
            let input = String::from_utf8(stdin.to_vec()).unwrap();
            let ignored: Vec<&str> = input.lines().filter(|l| l.starts_with("target/")).collect();
            self.stdins.lock().unwrap().push(input.clone());
            Ok(CommandOut {
                status: Some(if ignored.is_empty() { 1 } else { 0 }),
                stdout: ignored.join("\n"),
                stderr: String::new(),
            })
        }
    }

    fn paths(ps: &[&str]) -> Vec<PathBuf> {
        ps.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn j13_hook_signal_wakes_for_a_full_cycle() {
        let git = FakeGit::default();
        let got = classify(
            &git,
            Path::new("."),
            None,
            &paths(&["src/a.rs", ".wingman/watch/post-merge"]),
        );
        assert_eq!(got, Some(Wake::GitEvent));
        // Creating the signal directory itself is not an event.
        assert_eq!(
            classify(&git, Path::new("."), None, &paths(&[".wingman/watch"])),
            None
        );
    }

    #[test]
    fn j13_noise_never_wakes_the_daemon() {
        let git = FakeGit::default();
        let noise = paths(&[
            ".git/index",
            ".git/refs/heads/main",
            ".wingman/daemon-queue.jsonl",
            ".wingman/worktrees/t1/src/a.rs",
            "target/debug/wingman.exe",
        ]);
        assert_eq!(classify(&git, Path::new("."), None, &noise), None);
        // Only the path git might not ignore was asked about.
        assert_eq!(*git.stdins.lock().unwrap(), ["target/debug/wingman.exe\n"]);
    }

    #[test]
    fn j13_a_tracked_file_or_an_intake_drop_wakes_for_local_sources() {
        let git = FakeGit::default();
        let got = classify(
            &git,
            Path::new("."),
            None,
            &paths(&["target/x.o", "src/lib.rs"]),
        );
        assert_eq!(got, Some(Wake::FileChange));
        // An intake drop under .wingman/ counts even though git may ignore it,
        // and needs no git call.
        let git = FakeGit::default();
        let intake = Path::new(".wingman/intake");
        let got = classify(
            &git,
            Path::new("."),
            Some(intake),
            &paths(&[".wingman/intake/req.md"]),
        );
        assert_eq!(got, Some(Wake::FileChange));
        assert!(git.stdins.lock().unwrap().is_empty());
    }

    #[test]
    fn j13_ignore_checks_are_chunked_and_stop_at_the_first_relevant_path() {
        let git = FakeGit::default();
        let mut batch: Vec<PathBuf> = (0..600)
            .map(|i| PathBuf::from(format!("target/{i}.o")))
            .collect();
        assert_eq!(classify(&git, Path::new("."), None, &batch), None);
        assert_eq!(git.stdins.lock().unwrap().len(), 3);

        let git = FakeGit::default();
        batch.insert(10, PathBuf::from("src/main.rs"));
        assert_eq!(
            classify(&git, Path::new("."), None, &batch),
            Some(Wake::FileChange)
        );
        assert_eq!(git.stdins.lock().unwrap().len(), 1);
    }

    /// Real git on a full chunk of long ignored paths (~500 KiB): git echoes
    /// each one back while its stdin is still being written, past any
    /// platform's pipe buffers, so this stalls if stdin is written before
    /// stdout is drained.
    #[test]
    fn j13_a_build_flood_is_checked_against_real_git_without_hanging() {
        let repo = tempfile::tempdir().unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status();
        if !init.is_ok_and(|s| s.success()) {
            return; // no git on this machine
        }
        std::fs::write(repo.path().join(".gitignore"), "target/\n").unwrap();
        let flood: Vec<PathBuf> = (0..256)
            .map(|i| PathBuf::from(format!("target/debug/deps/{}-{i}.d", "x".repeat(2000))))
            .collect();
        let runner = crate::pr::SystemCommandRunner;
        assert_eq!(classify(&runner, repo.path(), None, &flood), None);
        assert_eq!(
            classify(&runner, repo.path(), None, &paths(&["src/lib.rs"])),
            Some(Wake::FileChange)
        );
    }

    #[test]
    fn j13_file_changes_run_only_local_sources() {
        let cfg = wingman_config::PilotDaemonConfig {
            sources: vec![
                "github_issues".into(),
                "todos".into(),
                "intake".into(),
                "ask".into(),
            ],
            ..Default::default()
        };
        assert_eq!(
            cycle_config(&cfg, Wake::FileChange).sources,
            ["todos", "intake", "ask"]
        );
        assert_eq!(cycle_config(&cfg, Wake::GitEvent).sources, cfg.sources);
        assert_eq!(cycle_config(&cfg, Wake::Poll).sources, cfg.sources);
    }

    #[test]
    fn j13_hook_script_names_the_binary_and_refuses_what_a_shebang_cannot_carry() {
        let exe = if cfg!(windows) {
            PathBuf::from(r"C:\tools\wingman.exe")
        } else {
            PathBuf::from("/usr/local/bin/wingman")
        };
        let script = hook_script(&exe).unwrap();
        // The binary itself is the interpreter: no shell in between.
        let expected = if cfg!(windows) {
            "#!C:/tools/wingman.exe"
        } else {
            "#!/usr/local/bin/wingman"
        };
        assert_eq!(script.lines().next(), Some(expected));
        assert_eq!(script.lines().nth(1), Some(HOOK_MARKER));
        assert!(hook_script(Path::new("/a\nb/wingman")).is_err());
        #[cfg(not(windows))]
        assert!(hook_script(Path::new("/opt/my tools/wingman")).is_err());
    }

    #[test]
    fn j13_install_leaves_foreign_hooks_alone_and_uninstall_removes_only_ours() {
        let repo = tempfile::tempdir().unwrap();
        let hooks = repo.path().join(".git").join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("post-merge"), "#!/bin/sh\necho mine\n").unwrap();
        let exe = std::env::current_exe().unwrap();
        // current_exe may sit under a path with spaces on a dev box; the
        // shebang rules are covered above, so only exercise install when it fits.
        if hook_script(&exe).is_err() {
            return;
        }
        let git = FakeGit::default();

        let first = install_hooks(&git, repo.path(), &exe).unwrap();
        assert!(
            repo.path().join(SIGNAL_DIR).is_dir(),
            "install marks the repo"
        );
        assert_eq!(first.installed.len(), HOOK_NAMES.len() - 1);
        assert_eq!(first.skipped, [hooks.join("post-merge")]);
        assert!(is_wingman_hook(&hooks.join("post-commit")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(hooks.join("post-commit"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "hook must be executable");
        }
        // Re-installing rewrites ours and still skips theirs.
        assert_eq!(install_hooks(&git, repo.path(), &exe).unwrap(), first);

        let removed = uninstall_hooks(&git, repo.path()).unwrap();
        assert_eq!(removed, first.installed);
        assert_eq!(
            std::fs::read_to_string(hooks.join("post-merge")).unwrap(),
            "#!/bin/sh\necho mine\n"
        );
        assert!(!hooks.join("post-commit").exists());
    }

    #[test]
    fn j13_hook_invocation_needs_our_marker_and_a_known_hook_name() {
        let dir = tempfile::tempdir().unwrap();
        let ours = dir.path().join("post-commit");
        std::fs::write(&ours, format!("#!/x/wingman\n{HOOK_MARKER}\n")).unwrap();
        let theirs = dir.path().join("post-merge");
        std::fs::write(&theirs, "#!/x/wingman\n# something else\n").unwrap();
        let renamed = dir.path().join("pre-commit");
        std::fs::write(&renamed, format!("#!/x/wingman\n{HOOK_MARKER}\n")).unwrap();

        let argv = |p: &Path| {
            vec![
                OsString::from("wingman"),
                p.as_os_str().to_owned(),
                "x".into(),
            ]
        };
        assert_eq!(hook_invocation(&argv(&ours)), Some("post-commit"));
        assert_eq!(hook_invocation(&argv(&theirs)), None);
        assert_eq!(hook_invocation(&argv(&renamed)), None);
        assert_eq!(
            hook_invocation(&[OsString::from("wingman"), "pilot".into()]),
            None
        );
        assert_eq!(hook_invocation(&[OsString::from("wingman")]), None);
    }

    #[test]
    fn j13_hook_event_is_recorded_only_in_a_main_worktree() {
        let main = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(main.path().join(".git")).unwrap();
        // A repo nobody installed into or watched (a shared hooksPath).
        record_hook_event(main.path(), "post-commit").unwrap();
        assert!(!main.path().join(".wingman").exists());

        std::fs::create_dir_all(main.path().join(SIGNAL_DIR)).unwrap();
        record_hook_event(main.path(), "post-commit").unwrap();
        assert!(main.path().join(SIGNAL_DIR).join("post-commit").is_file());

        // A linked worktree's `.git` is a file.
        let linked = tempfile::tempdir().unwrap();
        std::fs::write(linked.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        record_hook_event(linked.path(), "post-checkout").unwrap();
        assert!(!linked.path().join(".wingman").exists());
    }

    #[tokio::test]
    async fn j13_watcher_wakes_on_a_hook_signal_and_times_out_to_a_poll() {
        let repo = tempfile::tempdir().unwrap();
        let git = FakeGit::default();
        let mut watcher = Watcher::start(repo.path(), None, Duration::from_millis(100)).unwrap();

        let soon = tokio::time::Instant::now() + Duration::from_millis(300);
        assert_eq!(watcher.wait(&git, soon).await, Wake::Poll);

        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        record_hook_event(repo.path(), "post-commit").unwrap();
        // Generous: FSEvents on a loaded macOS runner can take seconds.
        let later = tokio::time::Instant::now() + Duration::from_secs(30);
        assert_eq!(watcher.wait(&git, later).await, Wake::GitEvent);
    }
}
