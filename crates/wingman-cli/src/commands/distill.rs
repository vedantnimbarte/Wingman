//! `wingman distill [--session <path>]` — extract durable project facts from
//! a past session into `.wingman/pending-memories.md` for review. Routed to
//! the fast model when one is configured.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use wingman_config::{Config, ProjectPaths};

use crate::runtime::{build_provider, resolve_selection};

pub async fn run(cfg: Config, session: Option<PathBuf>) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let paths = ProjectPaths::discover(&cwd);

    // Pick the session: explicit path, else the most recently modified one.
    let path = match session {
        Some(p) => p,
        None => match latest_session(&paths.sessions_dir) {
            Some(p) => p,
            None => {
                eprintln!("wingman: no sessions to distill yet");
                return Ok(ExitCode::SUCCESS);
            }
        },
    };

    let records = wingman_session::load_session(&path)
        .with_context(|| format!("load session {}", path.display()))?;
    let messages = wingman_session::records_to_messages(&records);
    if messages.is_empty() {
        eprintln!("wingman: session {} has no messages", path.display());
        return Ok(ExitCode::SUCCESS);
    }

    // Prefer the configured fast model for this cheap side call.
    let model_flag = cfg.router.fast_model.clone();
    let selection = resolve_selection(&cfg, model_flag.as_deref())?;
    let provider = build_provider(&cfg, &selection.provider_id)?;

    eprintln!(
        "wingman: distilling {} with {}/{} …",
        path.display(),
        selection.provider_id,
        selection.model
    );
    let n = wingman_learn::distill::distill_session(
        &provider,
        &selection.model,
        &messages,
        &paths.root,
    )
    .await?;

    let store = wingman_learn::distill::PendingStore::new(&paths.root);
    if n == 0 {
        println!("No new durable facts found.");
    } else {
        println!(
            "Staged {n} fact(s) for review in {}\nPromote the good ones with `save_memory` / `/remember`.",
            store.path().display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Most-recently-modified `*.jsonl` in `dir`, or None.
fn latest_session(dir: &std::path::Path) -> Option<PathBuf> {
    wingman_session::list_sessions(dir)
        .into_iter()
        .filter_map(|p| {
            let m = std::fs::metadata(&p).ok()?.modified().ok()?;
            Some((m, p))
        })
        .max_by_key(|(m, _)| *m)
        .map(|(_, p)| p)
}
