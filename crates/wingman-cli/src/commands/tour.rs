//! `wingman tour` — onboard onto an unfamiliar codebase.
//!
//! "Get me up to speed on this repo": architecture, entry points, key modules,
//! conventions, and where to start reading. Gathers structural signal (top-level
//! layout, manifests, likely entry points, language mix) and asks the model for
//! a concise orientation — grounded in the repo, not a generic essay. It has the
//! `semantic_search` / `find_symbol` tools available to dig deeper as needed.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use wingman_config::{global_config_path, Config, ProjectPaths};

pub async fn run(focus: Option<String>) -> Result<ExitCode> {
    let paths = ProjectPaths::discover(&std::env::current_dir()?);
    let context = gather_context(&paths.root);

    let focus_line = match &focus {
        Some(f) => format!("\nThe user especially wants to understand: {f}\n"),
        None => String::new(),
    };
    let prompt = format!(
        "You are onboarding a new engineer onto this codebase. Using the structural summary below \
         (and the `semantic_search` / `find_symbol` / `read_file` tools to confirm details), produce a \
         concise orientation:\n\
         1. What this project is and does (2-3 sentences).\n\
         2. High-level architecture — the main components and how they fit.\n\
         3. Entry points — where execution starts, where to put a breakpoint.\n\
         4. Key modules/files worth reading first, in order.\n\
         5. Conventions worth knowing (build/test commands, patterns, gotchas).\n\
         Be specific and reference real paths/symbols. Don't pad.{focus_line}\n\
         Repository: {}\n\n\
         STRUCTURAL SUMMARY:\n{context}\n",
        paths.root.display()
    );

    let cfg = load_config()?;
    let opts = crate::commands::headless::HeadlessOptions {
        prompt,
        json: false,
        mode_override: Some(wingman_config::PermissionMode::ReadOnly),
        model_override: None,
    };
    crate::commands::headless::run(cfg, opts).await
}

/// Build a compact structural summary of the repo: top-level entries, key
/// manifest/readme files, likely entry points, and a language file-count mix.
fn gather_context(root: &Path) -> String {
    let mut out = String::new();

    out.push_str("Top-level entries:\n");
    if let Ok(rd) = std::fs::read_dir(root) {
        let mut entries: Vec<(bool, String)> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with('.') && name != ".github" {
                    return None;
                }
                Some((e.path().is_dir(), name))
            })
            .collect();
        entries.sort();
        for (is_dir, name) in entries.iter().take(40) {
            out.push_str(&format!("  {}{}\n", name, if *is_dir { "/" } else { "" }));
        }
    }

    // Manifests / readmes give the project's identity + commands.
    out.push_str("\nManifests & docs found:\n");
    for f in [
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "Gemfile",
        "composer.json",
        "README.md",
        "CLAUDE.md",
        "WINGMAN.md",
        "Makefile",
    ] {
        if root.join(f).exists() {
            out.push_str(&format!("  {f}\n"));
        }
    }

    // Likely entry points.
    out.push_str("\nLikely entry points:\n");
    let mut entries = Vec::new();
    find_entry_points(root, root, 0, &mut entries);
    entries.sort();
    entries.dedup();
    for e in entries.iter().take(25) {
        out.push_str(&format!("  {e}\n"));
    }

    // Language mix (rough file counts by extension).
    out.push_str("\nLanguage mix (file counts):\n");
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    count_by_ext(root, 0, &mut counts);
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
    for (ext, n) in ranked.iter().take(12) {
        out.push_str(&format!("  .{ext}: {n}\n"));
    }

    out
}

fn is_ignored(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".wingman"
    )
}

fn find_entry_points(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > 3 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if is_ignored(&name) {
            continue;
        }
        if p.is_dir() {
            find_entry_points(root, &p, depth + 1, out);
        } else if matches!(
            name.as_str(),
            "main.rs"
                | "main.py"
                | "__main__.py"
                | "index.js"
                | "index.ts"
                | "main.go"
                | "app.py"
                | "server.js"
                | "cli.rs"
                | "lib.rs"
        ) {
            if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

fn count_by_ext(dir: &Path, depth: usize, counts: &mut std::collections::BTreeMap<String, usize>) {
    if depth > 5 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if is_ignored(&name) {
            continue;
        }
        if p.is_dir() {
            count_by_ext(&p, depth + 1, counts);
        } else if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
            if matches!(
                ext,
                "rs" | "py"
                    | "js"
                    | "ts"
                    | "tsx"
                    | "jsx"
                    | "go"
                    | "java"
                    | "c"
                    | "cpp"
                    | "h"
                    | "hpp"
                    | "rb"
                    | "cs"
                    | "php"
                    | "md"
                    | "toml"
                    | "json"
                    | "yaml"
                    | "yml"
            ) {
                *counts.entry(ext.to_string()).or_default() += 1;
            }
        }
    }
}

fn load_config() -> Result<Config> {
    let global = global_config_path()?;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let project_file: Option<PathBuf> = project.config_file.exists().then_some(project.config_file);
    Ok(Config::load(Some(&global), project_file.as_deref())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gather_context_reports_structure() {
        let dir = std::env::temp_dir().join(format!("wm-tour-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        let ctx = gather_context(&dir);
        assert!(ctx.contains("Cargo.toml"));
        assert!(ctx.contains("src/main.rs"));
        assert!(ctx.contains(".rs:"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
