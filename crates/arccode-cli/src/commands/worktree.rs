//! `arccode worktree …` — thin wrapper around `git worktree`.
//!
//! Lets the user spin up an isolated copy of the repo to run an arccode
//! session against, then clean it up. By convention worktrees live under
//! `<project>/.arccode/worktrees/<branch>` so they're easy to find and
//! gitignore-friendly.

use anyhow::{Context, Result};
use arccode_config::ProjectPaths;
use std::process::{Command, ExitCode, Stdio};

pub async fn create(branch: String) -> Result<ExitCode> {
    if branch.is_empty() {
        eprintln!("arccode: branch name is required");
        return Ok(ExitCode::from(1));
    }
    let cwd = std::env::current_dir()?;
    let project = ProjectPaths::discover(&cwd);
    let wt_root = project.dir.join("worktrees");
    std::fs::create_dir_all(&wt_root).ok();
    let safe_name = branch.replace(['/', '\\', ':'], "_");
    let dest = wt_root.join(&safe_name);

    if dest.exists() {
        eprintln!(
            "arccode: worktree path already exists: {} (remove it first or pick a different branch)",
            dest.display()
        );
        return Ok(ExitCode::from(1));
    }

    // Try `git worktree add -b <branch> <dest>`; if the branch already
    // exists, fall back to `git worktree add <dest> <branch>`.
    let status = Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            &branch,
            dest.to_str().unwrap_or_default(),
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("running `git worktree add -b`")?;
    if !status.success() {
        let status2 = Command::new("git")
            .args([
                "worktree",
                "add",
                dest.to_str().unwrap_or_default(),
                &branch,
            ])
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("running `git worktree add`")?;
        if !status2.success() {
            eprintln!("arccode: `git worktree add` failed for both new and existing branch");
            return Ok(ExitCode::from(1));
        }
    }

    println!("worktree ready at {}", dest.display());
    println!("  cd \"{}\" && arccode", dest.display());
    Ok(ExitCode::SUCCESS)
}

pub async fn list() -> Result<ExitCode> {
    let status = Command::new("git")
        .args(["worktree", "list"])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("running `git worktree list`")?;
    Ok(if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

pub async fn remove(path: String) -> Result<ExitCode> {
    let status = Command::new("git")
        .args(["worktree", "remove", &path])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("running `git worktree remove`")?;
    Ok(if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}
