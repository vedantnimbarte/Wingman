//! `wingman checkpoint` / `wingman undo` — git-stash based snapshot/restore.
//!
//! Each checkpoint is a `git stash push --include-untracked --keep-index` with
//! a recognizable label. `wingman undo` finds the most recent wingman stash
//! and `git stash pop`s it.

use anyhow::{Context, Result};
use std::process::{Command, ExitCode, Stdio};

const TAG: &str = "wingman-checkpoint";

pub async fn create(label: Option<String>) -> Result<ExitCode> {
    ensure_git_repo()?;
    let stamp = chrono_like_stamp();
    let msg = match label {
        Some(l) => format!("{TAG}: {l} @ {stamp}"),
        None => format!("{TAG}: {stamp}"),
    };
    let status = Command::new("git")
        .args([
            "stash",
            "push",
            "--include-untracked",
            "--keep-index",
            "-m",
            &msg,
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("running `git stash push`")?;
    if !status.success() {
        eprintln!("wingman: `git stash push` failed (nothing to stash? or hooks?)");
        return Ok(ExitCode::from(1));
    }
    println!("checkpoint saved: {msg}");
    Ok(ExitCode::SUCCESS)
}

pub async fn undo() -> Result<ExitCode> {
    ensure_git_repo()?;
    // Find the most recent stash whose message starts with our TAG.
    let out = Command::new("git")
        .args(["stash", "list"])
        .output()
        .context("running `git stash list`")?;
    if !out.status.success() {
        eprintln!("wingman: `git stash list` failed");
        return Ok(ExitCode::from(1));
    }
    let listing = String::from_utf8_lossy(&out.stdout);
    let idx = listing.lines().enumerate().find_map(|(i, line)| {
        if line.contains(TAG) {
            Some(format!("stash@{{{}}}", i))
        } else {
            None
        }
    });
    let Some(stash_ref) = idx else {
        eprintln!("wingman: no checkpoint stash found (look for '{TAG}' in `git stash list`)");
        return Ok(ExitCode::from(1));
    };
    let status = Command::new("git")
        .args(["stash", "pop", &stash_ref])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("running `git stash pop`")?;
    if !status.success() {
        eprintln!("wingman: `git stash pop {stash_ref}` failed (likely a conflict)");
        return Ok(ExitCode::from(1));
    }
    println!("restored {stash_ref}");
    Ok(ExitCode::SUCCESS)
}

fn ensure_git_repo() -> Result<()> {
    let out = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .context("running `git rev-parse`")?;
    if !out.status.success() {
        anyhow::bail!("not inside a git working tree");
    }
    Ok(())
}

fn chrono_like_stamp() -> String {
    // Avoid adding a `chrono` dep here — just use UNIX seconds.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("t{secs}")
}
