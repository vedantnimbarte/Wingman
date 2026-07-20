//! `wingman knows` — render what Wingman knows about this project: stored
//! memories, available skills, model routing, the verification gate, and
//! semantic-index freshness. Makes the accumulated knowledge visible so the
//! learning loop's value is obvious (and auditable) to the user.

use anyhow::Result;
use std::path::Path;
use std::process::ExitCode;
use wingman_config::{Config, ProjectPaths};

pub async fn run(cfg: Config) -> Result<ExitCode> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let paths = ProjectPaths::discover(&cwd);
    println!("What Wingman knows about {}", paths.root.display());
    println!();

    // Memories: global (~/.wingman/memory) + project (<root>/.wingman/memory).
    let global_mem = wingman_config::ensure_global_dir()
        .ok()
        .map(|d| d.join("memory"));
    print_memory_section("global memories", global_mem.as_deref());
    print_memory_section("project memories", Some(&paths.dir.join("memory")));

    // Staleness: memories naming project files that no longer exist.
    let store = wingman_learn::memory::MemoryStore::new(paths.root.clone());
    let all = store.load_all();
    let stale = wingman_learn::staleness::stale_memories(&all, &paths.root);
    if !stale.is_empty() {
        println!(
            "stale memories: {} (reference files that no longer exist)",
            stale.len()
        );
        for (m, missing) in stale.iter().take(10) {
            println!("  - {} → missing {}", m.name, missing.join(", "));
        }
        println!();
    }

    // Distilled facts awaiting review.
    let pending = wingman_learn::distill::PendingStore::new(&paths.root).load();
    if !pending.is_empty() {
        println!(
            "pending distilled facts: {} (review with `wingman distill`, promote via `save_memory`)",
            pending.len()
        );
    }

    // Skills (global + project, project wins on name clash).
    let skills = wingman_skills::load_all(&paths.root);
    println!("skills: {}", skills.len());
    for s in skills.iter().take(10) {
        println!("  - {} — {}", s.name, truncate(&s.description, 70));
    }
    if skills.len() > 10 {
        println!("  … and {} more", skills.len() - 10);
    }
    println!();

    // Model routing.
    let default_model = cfg
        .default_model
        .clone()
        .or_else(|| {
            cfg.default_provider.as_ref().and_then(|p| {
                cfg.providers
                    .get(p)
                    .and_then(|pc| pc.model.clone())
                    .map(|m| format!("{p}/{m}"))
            })
        })
        .unwrap_or_else(|| "(not configured)".into());
    println!("routing:");
    println!("  default model: {default_model}");
    println!(
        "  fast model:    {}",
        cfg.router.fast_model.as_deref().unwrap_or("(none)")
    );
    if cfg.router.classes.is_empty() {
        println!("  classes:       (none — all task classes use the default model)");
    } else {
        for (class, target) in &cfg.router.classes {
            println!("  class {class:<12} → {target}");
        }
    }
    if !cfg.router.fallback_models.is_empty() {
        println!("  fallbacks:     {}", cfg.router.fallback_models.join(", "));
    }
    println!();

    // Verification gate.
    match crate::runtime::build_turn_gate(&cfg, &paths.root) {
        Some(gate) => println!("turn gate: {}", gate.label()),
        None => println!(
            "turn gate: off ({})",
            if cfg.verify.turn_gate.trim() == "off" {
                "disabled in [verify]"
            } else {
                "no project check command detected"
            }
        ),
    }
    println!();

    // Semantic index freshness.
    if paths.index_db.exists() {
        let meta = std::fs::metadata(&paths.index_db).ok();
        let size_kb = meta.as_ref().map(|m| m.len() / 1024).unwrap_or(0);
        let age = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .map(|d| humanize(d.as_secs()))
            .unwrap_or_else(|| "unknown".into());
        println!(
            "semantic index: {} ({size_kb} KB, updated {age} ago)",
            paths.index_db.display()
        );
    } else {
        println!("semantic index: not built yet (runs on first semantic_search)");
    }

    // Learning databases.
    if let Ok(global) = wingman_config::ensure_global_dir() {
        for (label, file) in [
            ("skill stats", "learn.db"),
            ("session recall", "sessions.db"),
        ] {
            let p = global.join(file);
            if p.exists() {
                println!("{label}: {}", p.display());
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn print_memory_section(label: &str, dir: Option<&Path>) {
    let Some(dir) = dir else {
        println!("{label}: (no directory)");
        return;
    };
    let mut entries: Vec<(String, String)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let stem = p
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if stem == "MEMORY" {
                continue;
            }
            let desc = std::fs::read_to_string(&p)
                .ok()
                .and_then(|body| {
                    body.lines()
                        .find(|l| l.trim_start().starts_with("description:"))
                        .map(|l| {
                            l.trim_start()
                                .trim_start_matches("description:")
                                .trim()
                                .to_string()
                        })
                })
                .unwrap_or_default();
            entries.push((stem, desc));
        }
    }
    entries.sort();
    println!("{label}: {}", entries.len());
    for (name, desc) in entries.iter().take(10) {
        if desc.is_empty() {
            println!("  - {name}");
        } else {
            println!("  - {name} — {}", truncate(desc, 70));
        }
    }
    if entries.len() > 10 {
        println!("  … and {} more", entries.len() - 10);
    }
    println!();
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

fn humanize(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}
