//! `wingman router stats` — show which model wins per task class in this repo,
//! by verification-gate pass-rate. The raw outcomes are recorded per turn (see
//! the headless runner's `Verification` handling).

use std::process::ExitCode;

use anyhow::Result;
use wingman_config::ProjectPaths;

use crate::cli::RouterAction;

pub async fn run(action: RouterAction) -> Result<ExitCode> {
    match action {
        RouterAction::Stats { all } => stats(all).await,
        RouterAction::Preset { name, model } => preset(&name, model).await,
    }
}

/// Print a recommended `[router]` preset. The `local` preset keeps the cheap,
/// low-intelligence steps (summarize, compaction, commit-message, title,
/// search) on a local model so "simple steps never leave your machine" — a
/// privacy story a single-vendor agent structurally can't tell.
async fn preset(name: &str, model: Option<String>) -> Result<ExitCode> {
    match name {
        "local" => {
            let m = model.unwrap_or_else(|| "ollama/llama3.1".to_string());
            println!("# Local-first privacy preset. Paste into ~/.wingman/config.toml.");
            println!("# Cheap, low-intelligence steps run on your local model and never");
            println!("# leave the machine; reasoning/codegen stay on your session model.");
            println!();
            println!("[router]");
            println!("local_model = \"{m}\"");
            println!();
            println!("[router.classes]");
            for class in ["summarize", "search_summarize", "compaction", "commit_message", "title"] {
                println!("{class:<16} = \"local\"");
            }
            println!("{:<16} = \"default\"   # keep real thinking on the session model", "reason");
            println!("{:<16} = \"default\"", "codegen");
            println!();
            println!("# Requires a local server running (e.g. `ollama serve`) with the");
            println!("# model pulled. Run `wingman discover` to find local models.");
            Ok(ExitCode::SUCCESS)
        }
        other => {
            eprintln!("wingman: unknown preset '{other}' (available: local)");
            Ok(ExitCode::from(1))
        }
    }
}

async fn stats(all: bool) -> Result<ExitCode> {
    let store = match wingman_learn::StatsStore::open_default() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wingman: no stats db ({e})");
            return Ok(ExitCode::SUCCESS);
        }
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let paths = ProjectPaths::discover(&cwd);
    let repo = paths.root.to_string_lossy().to_string();
    let scope = if all { None } else { Some(repo.as_str()) };

    let rows = store.routing_summary(scope)?;
    if rows.is_empty() {
        println!(
            "No routing data yet{}. It accrues as you run sessions with the verification gate on.",
            if all { "" } else { " for this repo" }
        );
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "Routing win-rates{}:",
        if all { " (all repos)" } else { " (this repo)" }
    );
    let mut current = String::new();
    for r in &rows {
        if r.task_class != current {
            println!("\nclass: {}", r.task_class);
            current = r.task_class.clone();
        }
        println!(
            "  {:<40} {:>3}/{:<3}  {:>5.0}% pass",
            r.model,
            r.passed,
            r.total,
            r.pass_rate() * 100.0
        );
    }
    Ok(ExitCode::SUCCESS)
}
