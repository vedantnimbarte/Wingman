//! `wingman rewind` — scrub back through per-edit checkpoints.
//!
//! Every mutating tool call is auto-snapshotted (see
//! `wingman_core::checkpoint`), so this is the timeline over those snapshots:
//! `wingman rewind` (no args) prints it; `wingman rewind <n>` reverts the last
//! `n` edits, newest first. Distinct from `wingman undo`, which pops a
//! git-stash checkpoint.

use std::process::ExitCode;

use anyhow::Result;
use wingman_config::ProjectPaths;
use wingman_core::checkpoint;

pub async fn run(steps: Option<usize>) -> Result<ExitCode> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let paths = ProjectPaths::discover(&cwd);
    match steps {
        None => print_timeline(&paths.root),
        Some(n) => rewind(&paths.root, n),
    }
    Ok(ExitCode::SUCCESS)
}

fn print_timeline(root: &std::path::Path) {
    let steps = checkpoint::list(root);
    if steps.is_empty() {
        println!("No edits to rewind. (Each mutating tool call adds one step.)");
        return;
    }
    println!("Rewind timeline (newest first — `wingman rewind <n>` reverts the top n):");
    for (i, s) in steps.iter().enumerate() {
        let name = std::path::Path::new(&s.path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| s.path.clone());
        let kind = if s.existed { "edit" } else { "create" };
        let age = s.ts.map(age_str).unwrap_or_default();
        println!("  {i:>3}  {kind:<6}  {name:<32} {age}");
    }
}

fn rewind(root: &std::path::Path, n: usize) {
    let summaries = checkpoint::undo_n(root, n);
    if summaries.is_empty() {
        println!("Nothing to rewind.");
        return;
    }
    for s in &summaries {
        println!("↩ {s}");
    }
    if summaries.len() < n {
        println!("(reached the start of the timeline after {} step(s))", summaries.len());
    }
}

fn age_str(ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(ts);
    let secs = now.saturating_sub(ts);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}
