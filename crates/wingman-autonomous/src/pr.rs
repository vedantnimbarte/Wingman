//! Pull-request creation for a finished pilot run.
//!
//! Two paths, decided at runtime:
//!
//! - **`gh` present and authenticated** — run
//!   `gh pr create --base <base> --head <integration> --title <t> --body <b>`,
//!   capture the URL it prints, and persist [`Event::RunPr`].
//! - **`gh` missing or unauthenticated** — `git push -u origin <integration>`
//!   and print a compare URL so the user can open the PR by hand. Same
//!   `run.pr` event lands in `tasks.jsonl`, just with the compare-URL form.
//!
//! Either way the run terminates with [`Event::RunDone`] so the dashboard
//! and the resume path agree on "this run is finished."

use std::path::Path;
use std::process::Command;

use thiserror::Error;

use crate::model::{Event, RunState, TaskStatus};
use crate::store::{RunStore, StoreError};

#[derive(Debug, Error)]
pub enum PrError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("git push failed: {0}")]
    GitPush(String),
    #[error("gh pr create failed: {0}")]
    GhFailed(String),
    #[error("could not parse a remote `origin` URL from the repo")]
    NoOriginRemote,
    /// J15 — the push needed a force-push, and the branch is outside the
    /// pilot's `wingman/auto/*` namespace.
    #[error("{}", .0.render())]
    ForcePushRefused(crate::escalation::EscalationTrigger),
}

/// Outcome of [`open_pull_request`].
#[derive(Debug, Clone)]
pub struct PrOutcome {
    pub url: String,
    /// True if the URL is an open PR, false if it's just a compare URL
    /// (i.e. the `gh` fallback fired and the user must open the PR
    /// themselves).
    pub created_by_gh: bool,
}

/// Render the PR markdown body from the run state.
///
/// This is the minimal template — Phase 7.8 § E8 layers richer sections
/// on top (security findings, screenshot evidence, dangerous-paths
/// callout). For Phase 6 we just need the goal, the task list, and the
/// per-task outcome summaries.
pub fn render_pr_body(state: &RunState) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "## Goal\n\n{}\n", state.goal);

    let _ = writeln!(s, "## Tasks ({} total)\n", state.tasks.len());
    for t in &state.tasks {
        let _ = writeln!(
            s,
            "- **{id}** [{role}] {title} — {status:?}",
            id = t.id,
            role = t.role.as_str(),
            title = t.title,
            status = t.status,
        );
        if let Some(out) = &t.outcome {
            for line in out.summary.lines() {
                let _ = writeln!(s, "    {line}");
            }
            if !out.files_changed.is_empty() {
                let _ = writeln!(s, "    *files:* {}", out.files_changed.join(", "));
            }
        }
    }
    let _ = writeln!(s);

    if state.totals.usd > 0.0 || state.totals.tokens_in > 0 || state.totals.tokens_out > 0 {
        let _ = writeln!(
            s,
            "## Run cost\n\n- total: ${:.2}\n- tokens: in={} out={}\n",
            state.totals.usd, state.totals.tokens_in, state.totals.tokens_out
        );
    }

    let _ = writeln!(
        s,
        "_Opened by wingman pilot. Run id: `{}`. Base: `{}`._",
        state.run_id,
        &state.base_commit[..state.base_commit.len().min(8)]
    );
    s
}

/// Render a one-line PR title.
///
/// Format: `pilot(<run-id>): <goal first line>`. Title is capped so it
/// fits comfortably in a GitHub PR list view (~70 chars).
pub fn render_pr_title(state: &RunState) -> String {
    let first_line = state.goal.lines().next().unwrap_or("").trim();
    let truncated = if first_line.chars().count() > 60 {
        let mut out: String = first_line.chars().take(57).collect();
        out.push('…');
        out
    } else {
        first_line.to_string()
    };
    format!("pilot({}): {truncated}", state.run_id)
}

/// Abstraction over running shell commands. The default
/// [`SystemCommandRunner`] forwards to `std::process::Command`; tests
/// inject a [`MockCommandRunner`] so the gh-present / gh-missing /
/// push-failed paths are all exercisable without touching the network or
/// a real remote.
pub trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str], cwd: &Path) -> std::io::Result<CommandOut>;

    /// [`run`](Self::run) with `stdin` piped to the child (`ssh-keygen -Y
    /// verify` only reads the signed message from stdin). The default drops
    /// `stdin` so test doubles that only care about argv implement `run` alone.
    fn run_with_stdin(
        &self,
        program: &str,
        args: &[&str],
        cwd: &Path,
        stdin: &[u8],
    ) -> std::io::Result<CommandOut> {
        let _ = stdin;
        self.run(program, args, cwd)
    }
}

#[derive(Debug, Clone)]
pub struct CommandOut {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOut {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }
}

pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, program: &str, args: &[&str], cwd: &Path) -> std::io::Result<CommandOut> {
        let out = Command::new(program).args(args).current_dir(cwd).output()?;
        Ok(CommandOut {
            status: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn run_with_stdin(
        &self,
        program: &str,
        args: &[&str],
        cwd: &Path,
        stdin: &[u8],
    ) -> std::io::Result<CommandOut> {
        use std::io::Write as _;
        use std::process::Stdio;
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        // Written from a thread while `wait_with_output` drains stdout and
        // stderr. A program that answers line by line as it reads (`git
        // check-ignore --stdin`, fed a build's worth of paths by the J13
        // watcher) stops reading once nobody drains its full stdout pipe, so
        // writing everything first can deadlock. A write error (the child
        // exited early) shows up in its status instead.
        let pipe = child.stdin.take();
        let input = stdin.to_vec();
        let writer = std::thread::spawn(move || {
            if let Some(mut pipe) = pipe {
                let _ = pipe.write_all(&input);
            }
        });
        let out = child.wait_with_output();
        let _ = writer.join();
        let out = out?;
        Ok(CommandOut {
            status: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// Open a PR (or fall back to push + compare URL) for the integration
/// branch and persist `run.pr` + `run.done` events.
///
/// `base_branch` is the target (usually `main`). `gh_path` lets the
/// caller override the gh binary (mostly for the test seam); pass `None`
/// to use whichever `gh` is on PATH.
pub async fn open_pull_request(
    runner: &dyn CommandRunner,
    store: &mut RunStore,
    repo_root: &Path,
    base_branch: &str,
    integration_branch: &str,
    state: &RunState,
    gh_path: Option<&str>,
) -> Result<PrOutcome, PrError> {
    let title = render_pr_title(state);
    let body = render_pr_body(state);
    let outcome = open_pr(
        runner,
        repo_root,
        base_branch,
        integration_branch,
        &title,
        &body,
        gh_path,
    )?;

    store
        .append(Event::RunPr {
            t: RunStore::now(),
            url: outcome.url.clone(),
        })
        .await?;
    store.append(Event::RunDone { t: RunStore::now() }).await?;
    Ok(outcome)
}

/// Push `head` and open a PR against `base` with `gh`, or fall back to a push
/// and a compare URL when `gh` is missing or unauthenticated. No run store:
/// the half of [`open_pull_request`] that `wingman bg --pr` also uses.
pub fn open_pr(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    base_branch: &str,
    integration_branch: &str,
    title: &str,
    body: &str,
    gh_path: Option<&str>,
) -> Result<PrOutcome, PrError> {
    // Try `gh` first.
    let gh = gh_path.unwrap_or("gh");
    let gh_works = runner
        .run(gh, &["--version"], repo_root)
        .map(|o| o.success())
        .unwrap_or(false);

    if gh_works {
        // Confirm auth before attempting create; auth failures mid-create
        // are messier to handle. Treat any non-zero exit as "not
        // authenticated" and fall through to the push path.
        let auth = runner
            .run(gh, &["auth", "status"], repo_root)
            .map(|o| o.success())
            .unwrap_or(false);
        if auth {
            create_via_gh(
                runner,
                gh,
                repo_root,
                base_branch,
                integration_branch,
                title,
                body,
            )
        } else {
            tracing::info!(target: "pilot::pr", "gh present but not authenticated; falling back to push");
            fallback_push(runner, repo_root, integration_branch, base_branch)
        }
    } else {
        fallback_push(runner, repo_root, integration_branch, base_branch)
    }
}

fn create_via_gh(
    runner: &dyn CommandRunner,
    gh: &str,
    repo_root: &Path,
    base_branch: &str,
    integration_branch: &str,
    title: &str,
    body: &str,
) -> Result<PrOutcome, PrError> {
    // Push the integration branch first — gh pr create assumes the head
    // ref is reachable on the remote.
    push_branch(runner, repo_root, integration_branch)?;

    let out = runner.run(
        gh,
        &[
            "pr",
            "create",
            "--base",
            base_branch,
            "--head",
            integration_branch,
            "--title",
            title,
            "--body",
            body,
        ],
        repo_root,
    )?;
    if !out.success() {
        return Err(PrError::GhFailed(out.stderr.trim().to_string()));
    }
    let url = out
        .stdout
        .lines()
        .find_map(|l| {
            let l = l.trim();
            if l.starts_with("https://") {
                Some(l.to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| out.stdout.trim().to_string());
    Ok(PrOutcome {
        url,
        created_by_gh: true,
    })
}

/// Push `branch` to `origin`, force-pushing only where J15 allows it.
///
/// Every merge rebuilds the integration branch from the base commit, so a run
/// resumed after its branch was already pushed has new commits the remote copy
/// lacks and the plain push is rejected. Inside `wingman/auto/*` that branch
/// belongs to the pilot: it is replaced with `--force-with-lease` pinned to
/// the remote commit just read, so a push that raced in since is still not
/// overwritten, and only when that commit is one this clone has. Outside the
/// namespace the force-push is refused with the
/// [`crate::escalation::EscalationTrigger::ForcePushOutsideNamespace`] trigger.
fn push_branch(runner: &dyn CommandRunner, repo_root: &Path, branch: &str) -> Result<(), PrError> {
    let push = runner.run("git", &["push", "-u", "origin", branch], repo_root)?;
    if push.success() {
        return Ok(());
    }
    let stderr = push.stderr.trim().to_string();
    let needs_force = stderr.contains("[rejected]")
        && (stderr.contains("non-fast-forward") || stderr.contains("fetch first"));
    if !needs_force {
        return Err(PrError::GitPush(stderr));
    }
    if let Some(trigger) = crate::escalation::force_push_trigger(branch, true) {
        return Err(PrError::ForcePushRefused(trigger));
    }
    let remote_ref = format!("refs/heads/{branch}");
    let remote = runner.run("git", &["ls-remote", "origin", &remote_ref], repo_root)?;
    let Some(sha) = remote
        .stdout
        .split_whitespace()
        .next()
        .filter(|_| remote.success())
    else {
        return Err(PrError::GitPush(stderr));
    };
    // The lease only guards the moment between reading the remote and
    // pushing. A commit someone else pushed to the PR branch earlier (a review
    // fixup, GitHub's "update branch") is not in this clone, and a remote tip
    // this clone never had is left alone rather than overwritten.
    let tip = format!("{sha}^{{commit}}");
    if !runner
        .run("git", &["cat-file", "-e", &tip], repo_root)?
        .success()
    {
        return Err(PrError::GitPush(format!(
            "{stderr}\nremote `{branch}` is at {sha}, a commit this clone does not have, so it \
             was not force-pushed over; fetch it and reconcile by hand"
        )));
    }
    let lease = format!("--force-with-lease={remote_ref}:{sha}");
    tracing::warn!(
        target: "pilot::pr",
        branch,
        "remote branch has diverged (rebuilt integration branch); force-pushing with lease"
    );
    let forced = runner.run("git", &["push", "-u", &lease, "origin", branch], repo_root)?;
    if !forced.success() {
        return Err(PrError::GitPush(forced.stderr.trim().to_string()));
    }
    Ok(())
}

fn fallback_push(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    integration_branch: &str,
    base_branch: &str,
) -> Result<PrOutcome, PrError> {
    push_branch(runner, repo_root, integration_branch)?;
    let url = compose_compare_url(runner, repo_root, base_branch, integration_branch)?;
    eprintln!("[pilot] gh missing or unauthenticated. Open the PR by hand at:\n  {url}");
    Ok(PrOutcome {
        url,
        created_by_gh: false,
    })
}

/// Derive a github.com compare URL from the local `origin` remote.
/// Handles `git@github.com:org/repo.git` and `https://github.com/org/repo.git`
/// forms; returns [`PrError::NoOriginRemote`] for anything else (custom
/// hosts, multiple remotes, etc.) — the caller still gets the push URL
/// in stderr, just not a clickable link.
fn compose_compare_url(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    base_branch: &str,
    integration_branch: &str,
) -> Result<String, PrError> {
    let remote = runner.run("git", &["remote", "get-url", "origin"], repo_root)?;
    if !remote.success() {
        return Err(PrError::NoOriginRemote);
    }
    let url = remote.stdout.trim();
    let (org, repo) = parse_github_remote(url).ok_or(PrError::NoOriginRemote)?;
    Ok(format!(
        "https://github.com/{org}/{repo}/compare/{base_branch}...{integration_branch}?expand=1"
    ))
}

/// Parse `(org, repo)` from `git@github.com:org/repo.git` or
/// `https://github.com/org/repo[.git]` forms.
fn parse_github_remote(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim().trim_end_matches('/');
    // SSH: git@github.com:org/repo.git
    if let Some(rest) = raw.strip_prefix("git@github.com:") {
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        let (org, repo) = rest.split_once('/')?;
        return Some((org.into(), repo.into()));
    }
    // HTTPS: https://github.com/org/repo[.git]
    let stripped = raw
        .strip_prefix("https://github.com/")
        .or_else(|| raw.strip_prefix("http://github.com/"))?;
    let stripped = stripped.strip_suffix(".git").unwrap_or(stripped);
    let (org, repo) = stripped.split_once('/')?;
    Some((org.into(), repo.into()))
}

/// Mark every Review-status task in `state` as Done. Convenience for
/// callers that have already squash-merged via
/// [`crate::worktree::merge_integration`] and just need to flip statuses
/// without going through the manager-loop's finalize_task path.
pub async fn finalize_all_review_tasks(
    store: &mut RunStore,
    state: &RunState,
    merge_commits: &[crate::worktree::TaskMergeCommit],
) -> Result<(), StoreError> {
    let merge_by_id: std::collections::HashMap<&str, &str> = merge_commits
        .iter()
        .map(|c| (c.task_id.as_str(), c.commit_sha.as_str()))
        .collect();
    for task in &state.tasks {
        if task.status != TaskStatus::Review {
            continue;
        }
        if let Some(sha) = merge_by_id.get(task.id.as_str()) {
            store
                .append(Event::RunMergeTask {
                    t: RunStore::now(),
                    id: task.id.clone(),
                    strategy: "squash".into(),
                    commit: (*sha).to_string(),
                })
                .await?;
        } else {
            store
                .append(Event::TaskStatus {
                    t: RunStore::now(),
                    id: task.id.clone(),
                    status: TaskStatus::Done,
                    outcome: None,
                })
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Role, Task, TaskOutcome};
    use std::sync::Mutex;

    fn sample_state() -> RunState {
        let mut s = RunState::new(
            "2026-05-27-1430-abc",
            "add dark-mode toggle to the TUI",
            "deadbeefcafe1234",
            "wingman/auto/2026-05-27-1430-abc",
        );
        let mut t1 = Task::new("t1", Role::Developer, "Wire toggle key");
        t1.status = TaskStatus::Done;
        t1.outcome = Some(TaskOutcome {
            summary: "Added Ctrl+T binding in composer".into(),
            files_changed: vec!["crates/wingman-tui/src/composer.rs".into()],
        });
        s.tasks.push(t1);
        let mut t2 = Task::new("t2", Role::Designer, "Dark palette");
        t2.status = TaskStatus::Done;
        t2.outcome = Some(TaskOutcome {
            summary: "Added dark palette".into(),
            files_changed: vec!["crates/wingman-tui/src/theme.rs".into()],
        });
        t2.deps = vec!["t1".into()];
        s.tasks.push(t2);
        s.totals.usd = 0.42;
        s.totals.tokens_in = 12345;
        s.totals.tokens_out = 4567;
        s
    }

    #[test]
    fn pr_title_is_compact_and_run_qualified() {
        let s = sample_state();
        let title = render_pr_title(&s);
        assert!(title.starts_with("pilot(2026-05-27-1430-abc): "));
        assert!(title.contains("dark-mode"));
    }

    #[test]
    fn pr_title_truncates_long_goals() {
        let mut s = sample_state();
        s.goal = "a".repeat(200);
        let title = render_pr_title(&s);
        // 60 char goal cap + prefix overhead.
        assert!(title.chars().count() <= 90);
        assert!(
            title.contains("…")
                || !title
                    .contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn pr_body_contains_goal_and_task_summaries() {
        let s = sample_state();
        let body = render_pr_body(&s);
        assert!(body.contains("## Goal"));
        assert!(body.contains("add dark-mode toggle"));
        assert!(body.contains("**t1**"));
        assert!(body.contains("Added Ctrl+T binding"));
        assert!(body.contains("**t2**"));
        assert!(body.contains("Added dark palette"));
        assert!(body.contains("crates/wingman-tui/src/theme.rs"));
        assert!(body.contains("Run cost"));
        assert!(body.contains("Run id: `2026-05-27-1430-abc`"));
    }

    #[test]
    fn parse_github_remote_handles_ssh_and_https() {
        assert_eq!(
            parse_github_remote("git@github.com:vedant/wingman.git"),
            Some(("vedant".into(), "wingman".into())),
        );
        assert_eq!(
            parse_github_remote("https://github.com/vedant/wingman.git"),
            Some(("vedant".into(), "wingman".into())),
        );
        assert_eq!(
            parse_github_remote("https://github.com/vedant/wingman/"),
            Some(("vedant".into(), "wingman".into())),
        );
        assert_eq!(parse_github_remote("https://gitlab.com/x/y"), None);
        assert_eq!(parse_github_remote("file:///tmp/repo"), None);
    }

    /// CommandRunner that records every invocation and returns canned
    /// outputs keyed by `(program, args)`. Unmatched commands return a
    /// failing exit; the test asserts on `calls()` afterwards.
    struct MockCommandRunner {
        responses: Mutex<Vec<(String, Vec<String>, CommandOut)>>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl MockCommandRunner {
        fn new() -> Self {
            Self {
                responses: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn respond(&self, program: &str, args: &[&str], out: CommandOut) {
            self.responses.lock().unwrap().push((
                program.into(),
                args.iter().map(|s| s.to_string()).collect(),
                out,
            ));
        }
        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for MockCommandRunner {
        fn run(&self, program: &str, args: &[&str], _cwd: &Path) -> std::io::Result<CommandOut> {
            let key: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            self.calls
                .lock()
                .unwrap()
                .push((program.to_string(), key.clone()));
            let mut idx = None;
            for (i, (p, a, _)) in self.responses.lock().unwrap().iter().enumerate() {
                if p == program && *a == key {
                    idx = Some(i);
                    break;
                }
            }
            if let Some(i) = idx {
                let out = self.responses.lock().unwrap().remove(i).2;
                Ok(out)
            } else {
                Ok(CommandOut {
                    status: Some(127),
                    stdout: String::new(),
                    stderr: format!("no mock for {program} {key:?}"),
                })
            }
        }
    }

    fn ok(stdout: &str) -> CommandOut {
        CommandOut {
            status: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// Phase 6 acceptance (plan.md line 687): end-to-end PR open path.
    /// gh present + authenticated → gh pr create runs, URL captured,
    /// run.pr + run.done events written.
    #[tokio::test]
    async fn gh_present_path_creates_pr_and_emits_events() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = RunStore::create(
            dir.path(),
            "2026-05-27-1430-abc",
            "add dark-mode toggle to the TUI",
            "deadbeefcafe1234",
            "wingman/auto/2026-05-27-1430-abc",
        )
        .await
        .unwrap();
        // Replay the sample state's task.creates so render_pr_body has
        // data to template against.
        let s = sample_state();
        for t in &s.tasks {
            store
                .append(Event::TaskCreate {
                    t: RunStore::now(),
                    id: t.id.clone(),
                    role: t.role.clone(),
                    title: t.title.clone(),
                    goal: t.goal.clone(),
                    deps: t.deps.clone(),
                    writes: t.writes.clone(),
                    acceptance: t.acceptance.clone(),
                    reversibility: t.reversibility,
                    reversibility_reason: t.reversibility_reason.clone(),
                })
                .await
                .unwrap();
            store
                .append(Event::TaskStatus {
                    t: RunStore::now(),
                    id: t.id.clone(),
                    status: t.status,
                    outcome: t.outcome.clone(),
                })
                .await
                .unwrap();
        }
        let state = store.state().clone();

        let runner = MockCommandRunner::new();
        runner.respond("gh", &["--version"], ok("gh version 2.0.0"));
        runner.respond("gh", &["auth", "status"], ok("logged in"));
        runner.respond(
            "git",
            &["push", "-u", "origin", "wingman/auto/2026-05-27-1430-abc"],
            ok(""),
        );
        // gh prints the URL on stdout when it succeeds.
        runner.respond(
            "gh",
            &[
                "pr",
                "create",
                "--base",
                "main",
                "--head",
                "wingman/auto/2026-05-27-1430-abc",
                "--title",
                &render_pr_title(&state),
                "--body",
                &render_pr_body(&state),
            ],
            ok("https://github.com/vedant/wingman/pull/42\n"),
        );

        let outcome = open_pull_request(
            &runner,
            &mut store,
            dir.path(),
            "main",
            "wingman/auto/2026-05-27-1430-abc",
            &state,
            None,
        )
        .await
        .unwrap();

        assert_eq!(outcome.url, "https://github.com/vedant/wingman/pull/42");
        assert!(outcome.created_by_gh);

        // run.pr + run.done landed in the log.
        let log = std::fs::read_to_string(store.log_path()).unwrap();
        assert!(log.contains(r#""ev":"run.pr""#));
        assert!(log.contains(r#""ev":"run.done""#));
        assert!(log.contains("https://github.com/vedant/wingman/pull/42"));
        assert_eq!(
            store.state().pr_url.as_deref(),
            Some("https://github.com/vedant/wingman/pull/42")
        );
    }

    /// gh missing → push fallback. The remote URL is derived from `git
    /// remote get-url origin`, parsed into github org/repo, and the
    /// compare URL is recorded as the run.pr event.
    #[tokio::test]
    async fn gh_missing_path_falls_back_to_push_and_compare_url() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = RunStore::create(dir.path(), "r1", "a goal", "abc12345", "wingman/auto/r1")
            .await
            .unwrap();
        let state = store.state().clone();

        let runner = MockCommandRunner::new();
        // gh --version returns 127 → missing.
        runner.respond(
            "gh",
            &["--version"],
            CommandOut {
                status: Some(127),
                stdout: String::new(),
                stderr: "gh: command not found".into(),
            },
        );
        runner.respond("git", &["push", "-u", "origin", "wingman/auto/r1"], ok(""));
        runner.respond(
            "git",
            &["remote", "get-url", "origin"],
            ok("git@github.com:vedant/wingman.git\n"),
        );

        let outcome = open_pull_request(
            &runner,
            &mut store,
            dir.path(),
            "main",
            "wingman/auto/r1",
            &state,
            None,
        )
        .await
        .unwrap();

        assert!(!outcome.created_by_gh);
        assert!(outcome.url.contains("github.com/vedant/wingman"));
        assert!(outcome.url.contains("compare/main...wingman/auto/r1"));

        let calls = runner.calls();
        assert!(
            calls
                .iter()
                .any(|(p, a)| p == "git" && a == &["push", "-u", "origin", "wingman/auto/r1"]),
            "git push not invoked in fallback path: {calls:?}"
        );
    }

    fn rejected() -> CommandOut {
        CommandOut {
            status: Some(1),
            stdout: String::new(),
            stderr: " ! [rejected]        b -> b (non-fast-forward)
error: failed to push some refs"
                .into(),
        }
    }

    /// A resumed run rebuilds its integration branch, so the pushed copy has
    /// diverged. Inside `wingman/auto/*` that is replaced, leased to the remote
    /// commit just read so nothing pushed in between is overwritten.
    #[test]
    fn j15_diverged_pilot_branch_is_force_pushed_with_a_lease() {
        let runner = MockCommandRunner::new();
        runner.respond(
            "git",
            &["push", "-u", "origin", "wingman/auto/r1"],
            rejected(),
        );
        runner.respond(
            "git",
            &["ls-remote", "origin", "refs/heads/wingman/auto/r1"],
            ok("0123abcd	refs/heads/wingman/auto/r1
"),
        );
        runner.respond("git", &["cat-file", "-e", "0123abcd^{commit}"], ok(""));
        let lease = "--force-with-lease=refs/heads/wingman/auto/r1:0123abcd";
        runner.respond(
            "git",
            &["push", "-u", lease, "origin", "wingman/auto/r1"],
            ok(""),
        );
        push_branch(&runner, Path::new("."), "wingman/auto/r1").unwrap();
        assert!(runner
            .calls()
            .iter()
            .any(|(_, a)| a.iter().any(|x| x == lease)));
    }

    /// A remote tip this clone does not have was pushed by someone else (a
    /// review fixup on the PR); forcing would silently drop it.
    #[test]
    fn j15_a_remote_tip_this_clone_never_had_is_not_forced() {
        let runner = MockCommandRunner::new();
        runner.respond(
            "git",
            &["push", "-u", "origin", "wingman/auto/r1"],
            rejected(),
        );
        runner.respond(
            "git",
            &["ls-remote", "origin", "refs/heads/wingman/auto/r1"],
            ok("feedface\trefs/heads/wingman/auto/r1\n"),
        );
        // No cat-file response: the mock answers it with a failure.
        let err = push_branch(&runner, Path::new("."), "wingman/auto/r1").unwrap_err();
        assert!(
            matches!(&err, PrError::GitPush(m) if m.contains("does not have")),
            "{err}"
        );
        assert!(!runner
            .calls()
            .iter()
            .any(|(_, a)| a.iter().any(|x| x.starts_with("--force"))));
    }

    #[test]
    fn j15_force_push_outside_the_pilot_namespace_is_refused() {
        let runner = MockCommandRunner::new();
        runner.respond("git", &["push", "-u", "origin", "main"], rejected());
        let err = push_branch(&runner, Path::new("."), "main").unwrap_err();
        assert!(matches!(
            &err,
            PrError::ForcePushRefused(crate::escalation::EscalationTrigger::ForcePushOutsideNamespace { branch })
                if branch == "main"
        ));
        assert!(err.to_string().contains("refused to force-push `main`"));
        // Nothing past the rejected plain push ran.
        assert_eq!(runner.calls().len(), 1);
    }

    /// Any other push failure (auth, network) is reported, never forced.
    #[test]
    fn j15_other_push_failures_are_not_forced() {
        let runner = MockCommandRunner::new();
        runner.respond(
            "git",
            &["push", "-u", "origin", "wingman/auto/r1"],
            CommandOut {
                status: Some(128),
                stdout: String::new(),
                stderr: "fatal: could not read from remote repository".into(),
            },
        );
        let err = push_branch(&runner, Path::new("."), "wingman/auto/r1").unwrap_err();
        assert!(matches!(err, PrError::GitPush(_)));
        assert_eq!(runner.calls().len(), 1);
    }
}
