//! `arccode stats` — usage and learning metrics for the current project:
//! session counts, skill outcome scores, memory growth, and routing setup.
//! Companion to `arccode knows` (what is known) — this is how well it's
//! working.

use anyhow::Result;
use arccode_config::{Config, ProjectPaths};
use std::process::ExitCode;

pub async fn run(cfg: Config, json: bool) -> Result<ExitCode> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let paths = ProjectPaths::discover(&cwd);

    let sessions = arccode_session::list_sessions(&paths.sessions_dir);
    let memories = count_md(&paths.dir.join("memory"))
        + arccode_config::ensure_global_dir()
            .map(|d| count_md(&d.join("memory")))
            .unwrap_or(0);
    let skills = arccode_skills::load_all(&paths.root);

    let skill_summaries = arccode_learn::stats::StatsStore::open_default()
        .and_then(|s| s.summary())
        .unwrap_or_default();

    if json {
        let summary = serde_json::json!({
            "project": paths.root.display().to_string(),
            "sessions": sessions.len(),
            "memories": memories,
            "skills": skills.len(),
            "skill_outcomes": skill_summaries.iter().map(|s| serde_json::json!({
                "name": s.skill_name,
                "success": s.success,
                "corrected": s.corrected,
                "unclear": s.unclear,
                "total": s.total,
                "correction_rate": s.correction_rate(),
            })).collect::<Vec<_>>(),
            "router": {
                "fast_model": cfg.router.fast_model,
                "classes": cfg.router.classes,
            },
            "budget_max_usd_per_session": cfg.budget.max_usd_per_session,
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(ExitCode::SUCCESS);
    }

    println!("Arc-Code stats — {}", paths.root.display());
    println!();
    println!("sessions recorded: {}", sessions.len());
    println!("memories stored:   {memories}");
    println!("skills available:  {}", skills.len());
    println!();

    if skill_summaries.is_empty() {
        println!("skill outcomes: (none recorded yet)");
    } else {
        println!("skill outcomes (success / corrected / unclear):");
        for s in &skill_summaries {
            let flag = if s.needs_rewrite() { "  ⚠ needs rewrite" } else { "" };
            println!(
                "  {:<24} {:>3} / {:>3} / {:>3}  ({:.0}% correction rate){flag}",
                s.skill_name,
                s.success,
                s.corrected,
                s.unclear,
                s.correction_rate() * 100.0
            );
        }
    }
    println!();

    let routed = cfg.router.classes.len();
    println!(
        "routing: {} task class{} configured{}",
        routed,
        if routed == 1 { "" } else { "es" },
        cfg.router
            .fast_model
            .as_deref()
            .map(|m| format!(", fast model {m}"))
            .unwrap_or_default()
    );
    if cfg.budget.max_usd_per_session > 0.0 {
        println!(
            "budget:  ${:.2} hard ceiling per session",
            cfg.budget.max_usd_per_session
        );
    } else {
        println!("budget:  unlimited ([budget] max_usd_per_session not set)");
    }

    Ok(ExitCode::SUCCESS)
}

fn count_md(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    let p = e.path();
                    p.extension().and_then(|s| s.to_str()) == Some("md")
                        && p.file_stem().map(|s| s != "MEMORY").unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}
