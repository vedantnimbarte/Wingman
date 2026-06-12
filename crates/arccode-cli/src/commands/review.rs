//! `arccode review <pr#>` — fetch a PR diff via `gh` and run a one-shot
//! review prompt against the current default provider.
//!
//! Falls back to a local `git diff` against a base ref if `--local <base>`
//! is given (no `gh` required). The review template is intentionally
//! short — the user can supply their own via `--template <path>`.

use anyhow::{Context, Result};
use arccode_config::{global_config_path, Config, ProjectPaths};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

const DEFAULT_TEMPLATE: &str = "Review the following diff. For each finding, output:\n\
     - severity: blocker | major | minor | nit\n\
     - file:line\n\
     - 1-2 sentence explanation\n\
     Skip nits unless they are concrete bugs. Be specific; reference\n\
     actual lines. Don't restate the diff — only flag what's wrong or\n\
     risky.\n\n\
     DIFF:\n";

pub async fn run(
    pr: Option<String>,
    local_base: Option<String>,
    template: Option<String>,
) -> Result<ExitCode> {
    let diff = if let Some(pr) = pr {
        fetch_pr_diff(&pr)?
    } else if let Some(base) = local_base {
        fetch_local_diff(&base)?
    } else {
        eprintln!("arccode: pass <pr#> or --local <base-ref>");
        return Ok(ExitCode::from(1));
    };

    if diff.trim().is_empty() {
        eprintln!("arccode: no diff to review");
        return Ok(ExitCode::SUCCESS);
    }

    let template = match template {
        Some(path) => {
            std::fs::read_to_string(&path).with_context(|| format!("reading template {path}"))?
        }
        None => DEFAULT_TEMPLATE.to_string(),
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let diff = super::diff_annotate::annotate_diff_text(&cwd, &diff);
    let prompt = format!("{template}\n```\n{diff}\n```");

    let cfg = load_config()?;
    let mode_override = None;
    let opts = crate::commands::headless::HeadlessOptions {
        prompt,
        json: false,
        mode_override,
        worktree: false,
        model_override: None,
    };
    crate::commands::headless::run(cfg, opts).await
}

fn fetch_pr_diff(pr: &str) -> Result<String> {
    let out = Command::new("gh")
        .args(["pr", "diff", pr])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .context("running `gh pr diff` — is the GitHub CLI installed?")?;
    if !out.status.success() {
        anyhow::bail!("`gh pr diff {pr}` failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn fetch_local_diff(base: &str) -> Result<String> {
    let out = Command::new("git")
        .args(["diff", &format!("{base}...HEAD")])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .context("running `git diff`")?;
    if !out.status.success() {
        anyhow::bail!("`git diff {base}...HEAD` failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn load_config() -> Result<Config> {
    let global = global_config_path()?;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let project_file: Option<PathBuf> = if project.config_file.exists() {
        Some(project.config_file)
    } else {
        None
    };
    Ok(Config::load(Some(&global), project_file.as_deref())?)
}
