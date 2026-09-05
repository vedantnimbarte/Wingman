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

/// Where the notifier ought to be: beside `wingman` itself.
///
/// Resolved from `current_exe` rather than `PATH` so a locally built binary
/// launches its own sibling instead of an older installed one.
fn beside_this_exe() -> Option<PathBuf> {
    let path = std::env::current_exe().ok()?.parent()?.join(BIN);
    path.is_file().then_some(path)
}

pub fn run() -> Result<ExitCode> {
    if wingman_config::inbox::notifier_alive() {
        eprintln!("[notify] already running.");
        return Ok(ExitCode::SUCCESS);
    }

    let bin = beside_this_exe().ok_or_else(|| {
        anyhow!(
            "{BIN} not found next to the wingman binary.\n\
             It ships as a separate download; build it from source with:\n\
             \n    cargo build --release --manifest-path desktop/notifier/Cargo.toml\n"
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
    eprintln!("[notify] enable the cards you want in `[pilot.notifications]` and `[tools]` — see `docs/NOTIFIER.md`.");
    Ok(ExitCode::SUCCESS)
}
