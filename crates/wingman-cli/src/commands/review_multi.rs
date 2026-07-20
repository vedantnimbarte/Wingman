//! `wingman review-multi` — fan a code-review prompt out across multiple
//! `provider/model` pairs in parallel and merge findings.
//!
//! Each reviewer is asked to emit findings as one-per-line tagged with
//! `severity | file:line | message`. We dedupe by (file, line, gist of
//! message) so a finding raised by multiple reviewers shows as one row
//! with a "(n reviewers)" badge.

use anyhow::{Context, Result};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use wingman_config::{global_config_path, Config, PermissionMode, ProjectPaths};
use wingman_core::AgentEvent;

use crate::runtime::{self, Selection};

const REVIEWER_PROMPT: &str = "\
Review the following diff. Emit ONE LINE per finding, exactly in this format:\n\
<severity>|<file>:<line>|<message>\n\
where severity is one of: blocker, major, minor, nit. Skip nits unless\n\
they are concrete bugs. Reference real file paths and line numbers from\n\
the diff. Do not restate the diff. If you have no findings, output\n\
exactly: ok|-:-|no findings.\n\n\
DIFF:\n";

pub async fn run(
    pr: Option<String>,
    local_base: Option<String>,
    models_csv: String,
) -> Result<ExitCode> {
    let models: Vec<String> = models_csv
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if models.is_empty() {
        eprintln!("wingman: --models is empty");
        return Ok(ExitCode::from(1));
    }
    for m in &models {
        if !m.contains('/') {
            eprintln!("wingman: model entries must be provider/model — got '{m}'");
            return Ok(ExitCode::from(1));
        }
    }

    let diff = if let Some(pr) = pr {
        fetch_pr_diff(&pr)?
    } else if let Some(base) = local_base {
        fetch_local_diff(&base)?
    } else {
        eprintln!("wingman: pass <pr#> or --local <base-ref>");
        return Ok(ExitCode::from(1));
    };
    if diff.trim().is_empty() {
        eprintln!("wingman: no diff to review");
        return Ok(ExitCode::SUCCESS);
    }

    let cfg = load_config()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let diff = super::diff_annotate::annotate_diff_text(&cwd, &diff);
    let prompt = format!("{REVIEWER_PROMPT}\n```\n{diff}\n```");

    // Spawn one task per reviewer model. Each runs an isolated headless
    // agent loop and collects the assistant text.
    let mut handles = Vec::new();
    for raw in &models {
        let Some((p, m)) = raw.split_once('/') else {
            eprintln!("wingman: skipping malformed model '{raw}' (expected `provider/model`)");
            continue;
        };
        let sel = Selection {
            provider_id: p.to_string(),
            model: m.to_string(),
        };
        let cfg = cfg.clone();
        let prompt = prompt.clone();
        let label = raw.clone();
        handles.push(tokio::spawn(async move {
            let result = run_one(cfg, sel, prompt).await;
            (label, result)
        }));
    }

    let mut transcripts: Vec<(String, String)> = Vec::new();
    for h in handles {
        match h.await {
            Ok((label, Ok(text))) => transcripts.push((label, text)),
            Ok((label, Err(e))) => {
                eprintln!("[{label}] failed: {e}");
            }
            Err(e) => eprintln!("[task join] {e}"),
        }
    }
    if transcripts.is_empty() {
        eprintln!("wingman: all reviewers failed");
        return Ok(ExitCode::from(1));
    }

    let merged = merge_findings(&transcripts);
    print_merged(&merged, &transcripts);
    Ok(ExitCode::SUCCESS)
}

async fn run_one(cfg: Config, sel: Selection, prompt: String) -> Result<String> {
    let mode = PermissionMode::ReadOnly;
    let mut agent = runtime::build_agent_with_fallback(&cfg, &sel, mode).await?;
    let mut stream = agent.run(prompt);
    let mut out = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            AgentEvent::TextDelta { text } => out.push_str(&text),
            AgentEvent::Error { message } => anyhow::bail!(message),
            AgentEvent::Stop { .. } => break,
            _ => {}
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct Finding {
    severity: String,
    file: String,
    line: String,
    message: String,
}

fn parse_finding(line: &str) -> Option<Finding> {
    let parts: Vec<&str> = line.splitn(3, '|').collect();
    if parts.len() != 3 {
        return None;
    }
    let sev = parts[0].trim().to_ascii_lowercase();
    if !matches!(sev.as_str(), "blocker" | "major" | "minor" | "nit" | "ok") {
        return None;
    }
    let loc = parts[1].trim();
    let (file, ln) = loc.rsplit_once(':').unwrap_or((loc, "-"));
    Some(Finding {
        severity: sev,
        file: file.to_string(),
        line: ln.to_string(),
        message: parts[2].trim().to_string(),
    })
}

fn merge_findings(
    transcripts: &[(String, String)],
) -> BTreeMap<(String, String, String), (Finding, Vec<String>)> {
    let mut out: BTreeMap<(String, String, String), (Finding, Vec<String>)> = BTreeMap::new();
    for (label, text) in transcripts {
        for line in text.lines() {
            if let Some(f) = parse_finding(line) {
                if f.severity == "ok" {
                    continue;
                }
                let key = (
                    sev_sort_key(&f.severity),
                    f.file.clone(),
                    normalize_message(&f.message),
                );
                let entry = out.entry(key).or_insert_with(|| (f.clone(), Vec::new()));
                if !entry.1.contains(label) {
                    entry.1.push(label.clone());
                }
            }
        }
    }
    out
}

fn print_merged(
    merged: &BTreeMap<(String, String, String), (Finding, Vec<String>)>,
    transcripts: &[(String, String)],
) {
    println!("# Multi-model review\n");
    println!("Reviewers ({}):", transcripts.len());
    for (label, _) in transcripts {
        println!("  - {label}");
    }
    println!();
    if merged.is_empty() {
        println!("(no findings)");
        return;
    }
    println!("| sev     | file:line                                  | reviewers | message");
    println!("| ------- | ------------------------------------------ | --------- | -------");
    for (f, reviewers) in merged.values() {
        let loc = if f.line == "-" {
            f.file.clone()
        } else {
            format!("{}:{}", f.file, f.line)
        };
        let badge = if reviewers.len() == transcripts.len() {
            format!("all ({})", reviewers.len())
        } else {
            format!("{}/{}", reviewers.len(), transcripts.len())
        };
        println!(
            "| {:<7} | {:<42} | {:<9} | {}",
            f.severity,
            truncate(&loc, 42),
            badge,
            f.message,
        );
    }
}

fn sev_sort_key(s: &str) -> String {
    // Sort blocker < major < minor < nit alphabetically by inserting a digit.
    match s {
        "blocker" => "0-blocker",
        "major" => "1-major",
        "minor" => "2-minor",
        "nit" => "3-nit",
        _ => "9-other",
    }
    .to_string()
}

fn normalize_message(m: &str) -> String {
    // Strip leading "the/a/that", lowercase, collapse whitespace — so two
    // reviewers wording it differently still merge.
    let lower = m.to_ascii_lowercase();
    let stripped = lower
        .trim_start_matches("the ")
        .trim_start_matches("a ")
        .trim_start_matches("that ");
    stripped
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n - 1).collect();
        out.push('…');
        out
    }
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
