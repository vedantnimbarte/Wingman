//! `wingman explain` — explain-and-teach the working diff.
//!
//! Generates a concise, per-file "what changed and *why* it matters"
//! walkthrough of the current changes, aimed at a reviewer or a junior. Unlike
//! `review` (which hunts for problems), `explain` teaches the intent of the
//! change. Routes to the fast model when configured, so it's cheap.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use wingman_config::{global_config_path, Config, ProjectPaths};

const TEMPLATE: &str = "Explain the following diff as a teaching walkthrough for a reviewer or a \
    junior engineer. For EACH changed file, give:\n\
    - the file path,\n\
    - 1-2 sentences on WHAT changed, and\n\
    - 1-2 sentences on WHY it matters / the intent behind it.\n\
    Then one short overall summary line. Be concrete and reference real \
    symbols/behavior. Do NOT restate the diff line by line.\n\n\
    DIFF:\n";

pub async fn run(base: Option<String>, staged: bool) -> Result<ExitCode> {
    let diff = collect_diff(base.as_deref(), staged)?;
    if diff.trim().is_empty() {
        eprintln!("wingman: no changes to explain (clean working tree)");
        return Ok(ExitCode::SUCCESS);
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let annotated = super::diff_annotate::annotate_diff_text(&cwd, &diff);
    let prompt = format!("{TEMPLATE}\n```\n{annotated}\n```");

    let cfg = load_config()?;
    // Route to the fast model when configured — explanations don't need the
    // heavyweight model, keeping this cheap.
    let model_override = cfg.router.fast_model.clone();
    let opts = crate::commands::headless::HeadlessOptions {
        prompt,
        json: false,
        mode_override: None,
        model_override,
    };
    crate::commands::headless::run(cfg, opts).await
}

/// Collect the diff to explain: staged (`--staged`), against a base ref
/// (`--local <base>`), or all working-tree changes vs HEAD (default).
fn collect_diff(base: Option<&str>, staged: bool) -> Result<String> {
    let args: Vec<String> = if staged {
        vec!["diff".into(), "--staged".into()]
    } else if let Some(base) = base {
        vec!["diff".into(), format!("{base}...HEAD")]
    } else {
        vec!["diff".into(), "HEAD".into()]
    };
    let out = Command::new("git")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .context("running `git diff`")?;
    if !out.status.success() {
        anyhow::bail!("`git {}` failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn load_config() -> Result<Config> {
    let global = global_config_path()?;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let project_file = project.config_file.exists().then_some(project.config_file);
    Ok(Config::load(Some(&global), project_file.as_deref())?)
}
