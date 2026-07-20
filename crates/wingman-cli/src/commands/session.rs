//! `wingman session …` — list and fork session JSONL files.

use crate::cli::SessionAction;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::ExitCode;
use wingman_config::ProjectPaths;

pub async fn run(action: SessionAction) -> Result<ExitCode> {
    match action {
        SessionAction::List { limit } => list(limit).await,
        SessionAction::Fork { src, at } => fork(src, at).await,
    }
}

async fn list(limit: usize) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let project = ProjectPaths::discover(&cwd);
    let dir = project.sessions_dir.clone();
    if !dir.exists() {
        eprintln!("wingman: no sessions yet in {}", dir.display());
        return Ok(ExitCode::SUCCESS);
    }
    let mut entries: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                e.metadata().and_then(|m| m.modified()).ok().map(|t| (t, p))
            } else {
                None
            }
        })
        .collect();
    entries.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    for (_, p) in entries.into_iter().take(limit) {
        let lines = std::fs::read_to_string(&p)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        println!("{:<6} records  {}", lines, p.display());
    }
    Ok(ExitCode::SUCCESS)
}

async fn fork(src: String, at: Option<usize>) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let project = ProjectPaths::discover(&cwd);
    let src_path = PathBuf::from(&src);
    if !src_path.exists() {
        eprintln!("wingman: source session not found: {}", src_path.display());
        return Ok(ExitCode::from(1));
    }
    let dest = wingman_session::fork_session(&src_path, &project.sessions_dir, at)
        .await
        .context("fork_session")?;
    println!("forked to {}", dest.display());
    Ok(ExitCode::SUCCESS)
}
