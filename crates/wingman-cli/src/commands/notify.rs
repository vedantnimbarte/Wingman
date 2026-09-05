//! `wingman notify` — start the desktop notification popup.
//!
//! The popup is a separate binary (`desktop/notifier`, its own cargo
//! workspace — see `docs/decisions/0018-…`), so this is a launcher and nothing
//! more. It deliberately holds no channel to the app: everything they have to
//! say to each other goes through `~/.wingman/notifications.jsonl` and its
//! reply file, which is what lets a detached `pilot run` raise a card just as
//! easily as this process could.
//!
//! See `docs/NOTIFIER.md`.

use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::process::ExitCode;

/// Binary name, next to the `wingman` executable.
const BIN: &str = if cfg!(windows) {
    "wingman-notify.exe"
} else {
    "wingman-notify"
};

/// Where the notifier might be, best first.
///
/// A sibling of `wingman` itself comes first, resolved from `current_exe`
/// rather than `PATH`, so someone with a checkout launches the binary they just
/// built rather than an older installed one. The rest are where the bundler
/// puts it (decision 0019) — without them `wingman notify` cannot find a copy
/// the user double-clicked an installer for, which is most of the point of
/// having an installer.
fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
    {
        out.push(dir.join(BIN));
    }
    // NSIS, `installMode: currentUser`.
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        out.push(PathBuf::from(local).join("Wingman Notify").join(BIN));
    }
    #[cfg(target_os = "macos")]
    out.push(PathBuf::from("/Applications/Wingman Notify.app/Contents/MacOS").join(BIN));
    // `.deb` and AppImage both land on `PATH`; this is the prefix the deb uses,
    // checked directly because the caller wants a path to report, not a lookup.
    #[cfg(target_os = "linux")]
    out.push(PathBuf::from("/usr/bin").join(BIN));
    out
}

fn find_notifier() -> Option<PathBuf> {
    candidates().into_iter().find(|p| p.is_file())
}

pub fn run() -> Result<ExitCode> {
    if wingman_config::inbox::notifier_alive() {
        eprintln!("[notify] already running.");
        return Ok(ExitCode::SUCCESS);
    }

    // The one moment compaction is cheap and safe: the popup is provably not
    // running (checked just above), so the only writers left are pilot runs,
    // and `compact_if_large` abandons the attempt if one appends underneath.
    // Doing it here rather than in the app keeps the inbox format in one crate
    // — the notifier is a separate workspace that cannot see `wingman-config`.
    match wingman_config::inbox::compact_if_large_global() {
        Ok(true) => eprintln!("[notify] compacted the notification inbox."),
        Ok(false) => {}
        Err(e) => eprintln!("[notify] could not compact the inbox: {e}"),
    }

    let bin = find_notifier().ok_or_else(|| {
        anyhow!(
            "{BIN} not found next to the wingman binary.\n\
             Build it and put it here:\n\
             \n    cargo build --release --manifest-path desktop/notifier/Cargo.toml\n\
             \nor install it (unsigned -- see docs/NOTIFIER.md):\n\
             \n    npm --prefix desktop/notifier run bundle\n"
        )
    })?;

    // Detached: the popup outlives this command, which exits immediately. Its
    // output goes nowhere on purpose — a GUI app has no console to write to,
    // and inheriting this terminal's would scribble over whatever runs next.
    let mut cmd = std::process::Command::new(&bin);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no inherited console, and
        // Ctrl-C in this terminal must not take the popup down with it.
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let child = cmd
        .spawn()
        .map_err(|e| anyhow!("could not start {}: {e}", bin.display()))?;

    eprintln!("[notify] started (pid {}).", child.id());
    for line in hints(load_config().ok().as_ref()) {
        eprintln!("[notify] {line}");
    }
    Ok(ExitCode::SUCCESS)
}

fn load_config() -> Result<wingman_config::Config> {
    let global = wingman_config::global_config_path()?;
    let project = wingman_config::ProjectPaths::discover(&std::env::current_dir()?);
    let project_file = project.config_file.exists().then_some(project.config_file);
    Ok(wingman_config::Config::load(
        Some(&global),
        project_file.as_deref(),
    )?)
}

/// What to say after starting, given the config that is actually in force.
///
/// The generic "go and configure it" line was not enough for one case in
/// particular: `progress` routes to the digest by default, so someone who turns
/// `desktop_inbox` on gets cards for gates and failures but none for a run that
/// finishes — and reads that as the feature being broken rather than as the
/// setting it is. Saying so at the moment they start the app costs nothing and
/// is where they are looking.
///
/// Split out from `run` so it is testable without spawning anything.
fn hints(cfg: Option<&wingman_config::Config>) -> Vec<String> {
    use wingman_autonomous::notify::{desktop_target, NotificationSeverity};

    let Some(cfg) = cfg else {
        return vec![
            "enable the cards you want in `[pilot.notifications]` and `[tools]` — see `docs/NOTIFIER.md`."
                .into(),
        ];
    };
    let n = &cfg.pilot.notifications;
    let on = |s| desktop_target(s, n).is_some();

    if !on(NotificationSeverity::Escalation) && !on(NotificationSeverity::Decision) {
        return vec![
            "no cards are routed to the desktop yet — set `[pilot.notifications].desktop_inbox = true` and see `docs/NOTIFIER.md`."
                .into(),
        ];
    }

    let mut out = Vec::new();
    if !on(NotificationSeverity::Progress) {
        out.push(
            "note: `progress` is not routed here, so a run that finishes cleanly will not raise a card. Set `[pilot.notifications].progress = \"desktop\"` if you want those too."
                .into(),
        );
    }
    if cfg.tools.ask_user_desktop_timeout_secs == 0 {
        out.push(
            "note: `[tools].ask_user_desktop_timeout_secs = 0`, so the agent's questions still go to the terminal rather than here."
                .into(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wingman_config::Config;

    #[test]
    fn a_sibling_binary_is_preferred_over_an_installed_one() {
        let c = candidates();
        assert!(!c.is_empty());
        // The checkout wins: whoever has one is running what they just built.
        let exe = std::env::current_exe().unwrap();
        assert_eq!(c[0].parent().unwrap(), exe.parent().unwrap());
        assert!(c.iter().all(|p| p.file_name() == Some(BIN.as_ref())));
    }

    #[cfg(windows)]
    #[test]
    fn the_installer_location_is_searched_too() {
        // Without this the installer from 0019 produces something
        // `wingman notify` cannot find.
        std::env::set_var("LOCALAPPDATA", "C:\\tmp");
        let c = candidates();
        assert!(
            c.iter()
                .any(|p| p.to_string_lossy().contains("Wingman Notify")),
            "{c:?}"
        );
    }

    fn cfg(desktop: bool, progress: &str, ask: u64) -> Config {
        let mut c = Config::default();
        c.pilot.notifications.desktop_inbox = desktop;
        c.pilot.notifications.progress = progress.into();
        c.tools.ask_user_desktop_timeout_secs = ask;
        c
    }

    #[test]
    fn with_no_config_it_falls_back_to_the_generic_line() {
        let out = hints(None);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("NOTIFIER.md"));
    }

    #[test]
    fn it_says_so_when_nothing_is_routed_here_at_all() {
        // The defaults. Starting the app in this state shows nothing ever, and
        // that is the first thing worth knowing.
        let out = hints(Some(&cfg(false, "digest", 0)));
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("desktop_inbox = true"), "{out:?}");
    }

    #[test]
    fn it_warns_that_a_clean_run_will_not_card() {
        // The case that reads as a bug: cards on, but `progress` digests, so a
        // run that finishes says nothing.
        let out = hints(Some(&cfg(true, "digest", 0)));
        assert!(
            out.iter().any(|l| l.contains("finishes cleanly")),
            "{out:?}"
        );
    }

    #[test]
    fn routing_progress_here_removes_that_warning() {
        let out = hints(Some(&cfg(true, "desktop", 30)));
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn it_mentions_questions_still_going_to_the_terminal() {
        let out = hints(Some(&cfg(true, "desktop", 0)));
        assert!(
            out.iter()
                .any(|l| l.contains("ask_user_desktop_timeout_secs")),
            "{out:?}"
        );
    }
}
