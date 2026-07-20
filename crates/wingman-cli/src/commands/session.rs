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
        SessionAction::Replay { src } => replay(src).await,
    }
}

/// Re-run a past session's user prompts against the current code — reproduce
/// what happened, for debugging / regression. (Deterministic replay of the
/// provider's *outputs* is separate; this replays the inputs.)
async fn replay(src: String) -> Result<ExitCode> {
    let src_path = PathBuf::from(&src);
    let text = std::fs::read_to_string(&src_path)
        .with_context(|| format!("read session {}", src_path.display()))?;
    // Extract user prompts in order from the JSONL.
    let prompts: Vec<String> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("user"))
        .filter_map(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
        .collect();
    if prompts.is_empty() {
        eprintln!("wingman: no user prompts found in {}", src_path.display());
        return Ok(ExitCode::from(1));
    }
    let total = prompts.len();
    eprintln!(
        "replaying {total} prompt(s) from {} (read-only)",
        src_path.display()
    );
    let cfg = load_config()?;
    for (i, prompt) in prompts.into_iter().enumerate() {
        eprintln!("\n=== replay {}/{total} ===", i + 1);
        let opts = crate::commands::headless::HeadlessOptions {
            prompt,
            json: false,
            mode_override: Some(wingman_config::PermissionMode::ReadOnly),
            model_override: None,
        };
        // Best-effort reproduction; keep going across prompts.
        let _ = crate::commands::headless::run(cfg.clone(), opts).await?;
    }
    Ok(ExitCode::SUCCESS)
}

fn load_config() -> Result<wingman_config::Config> {
    let global = wingman_config::global_config_path()?;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let project_file = project.config_file.exists().then_some(project.config_file);
    Ok(wingman_config::Config::load(
        Some(&global),
        project_file.as_deref(),
    )?)
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
