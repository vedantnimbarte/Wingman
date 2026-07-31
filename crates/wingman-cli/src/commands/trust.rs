//! `wingman trust` — grant, revoke, and inspect trust for a project's
//! `.wingman/config.toml`.
//!
//! A project config ships with whatever repository you cloned, so Wingman only
//! honours its executable keys (`[hooks]`, `[mcp]`, `[[tools.custom]]`,
//! `[verify]`, `[providers]`, `permission_mode`, …) once you have said this
//! exact file content is yours. Trust is pinned to a SHA-256 of the file, so
//! any later edit — by you or by a `git pull` — revokes it until re-granted.

use crate::cli::TrustAction;
use anyhow::Result;
use std::process::ExitCode;
use wingman_config::{trust, ProjectPaths};

pub async fn run(action: Option<TrustAction>) -> Result<ExitCode> {
    match action.unwrap_or(TrustAction::Add) {
        TrustAction::Add => add(),
        TrustAction::Remove => remove(),
        TrustAction::Show => show(),
        TrustAction::List => list(),
    }
}

fn project_config() -> Result<std::path::PathBuf> {
    let cwd = std::env::current_dir()?;
    Ok(ProjectPaths::discover(&cwd).config_file)
}

fn add() -> Result<ExitCode> {
    let path = project_config()?;
    if !path.exists() {
        eprintln!("wingman: no project config at {}", path.display());
        eprintln!("         nothing to trust — create it first.");
        return Ok(ExitCode::from(1));
    }

    // Show the user what they are about to authorise. Executable keys are the
    // whole point of the prompt, so name them explicitly.
    let text = std::fs::read_to_string(&path)?;
    let table: toml::Table = text.parse().unwrap_or_default();
    let notable: Vec<&str> = [
        "hooks",
        "mcp",
        "verify",
        "providers",
        "permission_mode",
        "team",
        "audit",
        "privacy",
        "pilot",
        "schedule",
    ]
    .into_iter()
    .filter(|k| table.contains_key(*k))
    .collect();

    let hash = trust::trust(&path)?;
    println!("Trusted {}", path.display());
    println!("  sha256: {hash}");
    if notable.is_empty() {
        println!("  (no executable keys — this file was already fully honoured)");
    } else {
        println!("  now honoured from this file: {}", notable.join(", "));
    }
    println!("\nAny edit to this file revokes trust until you run `wingman trust` again.");
    Ok(ExitCode::SUCCESS)
}

fn remove() -> Result<ExitCode> {
    let path = project_config()?;
    if trust::untrust(&path)? {
        println!("Revoked trust for {}", path.display());
    } else {
        println!("{} was not trusted", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

fn show() -> Result<ExitCode> {
    let path = project_config()?;
    if !path.exists() {
        println!("no project config at {}", path.display());
        return Ok(ExitCode::SUCCESS);
    }
    if trust::is_trusted(&path) {
        println!("trusted: {}", path.display());
        Ok(ExitCode::SUCCESS)
    } else {
        let reason = if trust::recorded_hash(&path).is_some() {
            "was trusted, but the file has changed since"
        } else {
            "never trusted"
        };
        println!("NOT trusted: {} ({reason})", path.display());
        println!("executable keys in this file are being ignored.");
        Ok(ExitCode::from(1))
    }
}

fn list() -> Result<ExitCode> {
    let entries = trust::list();
    if entries.is_empty() {
        println!("no trusted project configs");
        return Ok(ExitCode::SUCCESS);
    }
    for (path, hash) in entries {
        let stale = trust::hash_file(std::path::Path::new(&path))
            .map(|h| h != hash)
            .unwrap_or(true);
        let mark = if stale {
            " (stale — file changed)"
        } else {
            ""
        };
        println!("{path}{mark}");
    }
    Ok(ExitCode::SUCCESS)
}
