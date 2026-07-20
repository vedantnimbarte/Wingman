//! `wingman pr address` — PR-native workflow.
//!
//! Close the loop after a PR is opened: pull the review comments and failing CI
//! checks for a PR, then run an agent turn that addresses them on the current
//! branch. Turns the human review round-trip — "address these comments and fix
//! CI" — into one command. Needs `gh`.

use anyhow::{Context, Result};
use std::process::{Command, ExitCode, Stdio};
use wingman_config::{global_config_path, Config, PermissionMode, ProjectPaths};

pub async fn address(pr: String) -> Result<ExitCode> {
    let comments = fetch_review_feedback(&pr)?;
    let checks = fetch_failing_checks(&pr);

    if comments.trim().is_empty() && checks.trim().is_empty() {
        println!("PR #{pr}: no review comments and no failing checks — nothing to address.");
        return Ok(ExitCode::SUCCESS);
    }

    let prompt = format!(
        "Address the feedback on PR #{pr} on the current branch. Make the changes, keep them minimal \
         and focused, and ensure the verification gate passes. For each item, do the change (or, if you \
         disagree, explain why in your summary rather than silently skipping).\n\n\
         REVIEW COMMENTS:\n{}\n\n\
         FAILING CHECKS:\n{}\n",
        if comments.trim().is_empty() { "(none)" } else { &comments },
        if checks.trim().is_empty() { "(none)" } else { &checks },
    );

    let cfg = load_config()?;
    let opts = crate::commands::headless::HeadlessOptions {
        prompt,
        json: false,
        mode_override: Some(PermissionMode::AutoEdit),
        model_override: None,
    };
    crate::commands::headless::run(cfg, opts).await
}

/// Review threads + top-level review bodies, via `gh pr view --json`.
fn fetch_review_feedback(pr: &str) -> Result<String> {
    let out = Command::new("gh")
        .args(["pr", "view", pr, "--json", "reviews,comments"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .context("running `gh pr view` — is the GitHub CLI installed and authenticated?")?;
    if !out.status.success() {
        anyhow::bail!("`gh pr view {pr}` failed");
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let mut s = String::new();
    for field in ["reviews", "comments"] {
        if let Some(arr) = v.get(field).and_then(|x| x.as_array()) {
            for item in arr {
                let author = item
                    .get("author")
                    .and_then(|a| a.get("login"))
                    .and_then(|l| l.as_str())
                    .unwrap_or("reviewer");
                let body = item.get("body").and_then(|b| b.as_str()).unwrap_or("");
                if !body.trim().is_empty() {
                    s.push_str(&format!("- @{author}: {}\n", body.trim()));
                }
            }
        }
    }
    Ok(s)
}

/// Names + links of failing CI checks, via `gh pr checks`.
fn fetch_failing_checks(pr: &str) -> String {
    let out = Command::new("gh")
        .args(["pr", "checks", pr])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else {
        return String::new();
    };
    // `gh pr checks` exits non-zero when checks fail; parse regardless.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("\tfail") || l.to_lowercase().contains("fail"))
        .map(|l| format!("- {}\n", l.split('\t').next().unwrap_or(l).trim()))
        .collect()
}

fn load_config() -> Result<Config> {
    let global = global_config_path()?;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let project_file = project.config_file.exists().then_some(project.config_file);
    Ok(Config::load(Some(&global), project_file.as_deref())?)
}
