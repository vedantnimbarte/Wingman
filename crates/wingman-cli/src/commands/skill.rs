//! `wingman skill extract` — auto-propose skill drafts from session history.

use anyhow::Result;
use wingman_config::ProjectPaths;
use wingman_learn::extract::{extract_from_dir, write_drafts, ExtractConfig};
use std::process::ExitCode;

pub async fn extract(min: usize, force: bool) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let project = ProjectPaths::discover(&cwd);
    let cfg = ExtractConfig {
        min_occurrences: min.max(2),
        ..Default::default()
    };
    let patterns = extract_from_dir(&project.sessions_dir, &cfg);
    if patterns.is_empty() {
        eprintln!(
            "wingman: no repeated tool-call patterns found (scanned {})",
            project.sessions_dir.display()
        );
        return Ok(ExitCode::SUCCESS);
    }

    println!("found {} candidate pattern(s):", patterns.len());
    for (i, p) in patterns.iter().enumerate() {
        println!(
            "{:>3}. ×{:<3}  {}",
            i + 1,
            p.occurrences,
            p.sequence.join(" → ")
        );
    }

    let proposed_dir = wingman_config::ensure_global_dir()?
        .join("skills")
        .join("proposed");
    let written = write_drafts(&proposed_dir, &patterns, force)?;
    if written.is_empty() {
        println!("\n(no drafts written — all candidates already exist; pass --force to overwrite)");
    } else {
        println!(
            "\nwrote {} draft(s) to {}",
            written.len(),
            proposed_dir.display()
        );
        println!("review and move into ~/.wingman/skills/ to promote.");
    }
    Ok(ExitCode::SUCCESS)
}
