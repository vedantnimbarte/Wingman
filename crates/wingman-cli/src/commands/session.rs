//! `wingman session …` — list, fork, replay and export session JSONL files.

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
        SessionAction::Export { id, format, output } => export(id, format, output),
    }
}

/// Render a session as a report and print it, or write it to `output`.
///
/// `id` is a session id under this project's sessions directory, or a path to
/// any session JSONL (a worker's, a colleague's), so the same command covers
/// "the session I just had" and "the transcript someone sent me".
fn export(id: String, format: String, output: Option<PathBuf>) -> Result<ExitCode> {
    let format: wingman_session::export::Format = format.parse().map_err(anyhow::Error::msg)?;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let Some(path) = resolve_session(&project.sessions_dir, &id) else {
        eprintln!(
            "wingman: no session '{id}' in {} (and no such file)",
            project.sessions_dir.display()
        );
        return Ok(ExitCode::from(1));
    };
    let export = wingman_session::export::export_file(&path)
        .with_context(|| format!("read session {}", path.display()))?;
    let text = export.render(format);
    match output {
        Some(out) => {
            std::fs::write(&out, &text).with_context(|| format!("write {}", out.display()))?;
            eprintln!("exported {} to {}", export.session_id, out.display());
        }
        None => print!("{text}"),
    }
    if export.redacted > 0 {
        eprintln!("wingman: redacted {} secret(s)", export.redacted);
    }
    Ok(ExitCode::SUCCESS)
}

/// A session id under `sessions_dir` first, then a path.
fn resolve_session(sessions_dir: &std::path::Path, id: &str) -> Option<PathBuf> {
    wingman_session::session_path(sessions_dir, id)
        .or_else(|| Some(PathBuf::from(id)).filter(|p| p.is_file()))
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
            // Replay deliberately starts each prompt fresh: the point is to
            // reproduce prompts against current code, not to continue a chat.
            session_id: None,
            resume: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_export_names_a_session_by_id_or_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("20260914T101500000Z.jsonl");
        std::fs::write(&log, "").unwrap();
        assert_eq!(
            resolve_session(dir.path(), "20260914T101500000Z"),
            Some(log.clone())
        );
        let by_path = log.to_string_lossy().to_string();
        assert_eq!(resolve_session(dir.path(), &by_path), Some(log));
        assert_eq!(resolve_session(dir.path(), "nope"), None);
    }
}
