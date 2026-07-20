//! `wingman bench` — internal benchmark harness.
//!
//! Runs a suite of prompts headlessly and records the metrics that the
//! differentiation work is supposed to move: **time to first useful token**,
//! **tokens per task**, **wall time**, and **verified-done rate** (did the
//! turn gate pass). Needs a live provider to run — it's a local measurement
//! tool, not a CI job — but the suite parsing and metric aggregation are pure
//! and unit-tested.
//!
//! Suite format: a JSONL file, one `{ "id": "...", "prompt": "..." }` per line.
//! With no `--suite`, a tiny built-in read-only suite runs.

use anyhow::Result;
use futures::StreamExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;
use wingman_config::{global_config_path, Config, PermissionMode, ProjectPaths};
use wingman_core::AgentEvent;

fn load_config() -> Result<Config> {
    let global = global_config_path()?;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let project_file: Option<PathBuf> = project.config_file.exists().then_some(project.config_file);
    Ok(Config::load(Some(&global), project_file.as_deref())?)
}

/// One task in a benchmark suite.
#[derive(Debug, Clone)]
pub struct BenchTask {
    pub id: String,
    pub prompt: String,
}

/// Metrics captured for one task run.
#[derive(Debug, Clone)]
pub struct TaskResult {
    pub id: String,
    pub first_token_ms: Option<u128>,
    pub wall_ms: u128,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub verified: Option<bool>,
    pub errored: bool,
}

pub async fn run(suite_path: Option<String>, json: bool) -> Result<ExitCode> {
    let tasks = match &suite_path {
        Some(p) => parse_suite(&std::fs::read_to_string(p)?)?,
        None => builtin_suite(),
    };
    if tasks.is_empty() {
        eprintln!("wingman: empty benchmark suite");
        return Ok(ExitCode::from(1));
    }
    let cfg = load_config()?;

    eprintln!("wingman bench: running {} task(s)…", tasks.len());
    let mut results = Vec::new();
    for task in &tasks {
        results.push(run_one(&cfg, task).await);
    }

    if json {
        let rows: Vec<serde_json::Value> = results.iter().map(result_to_json).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "tasks": rows,
                "summary": summarize(&results),
            }))?
        );
    } else {
        print_table(&results);
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_one(cfg: &Config, task: &BenchTask) -> TaskResult {
    // Read-only so a benchmark can't mutate the repo it runs in.
    let mode = PermissionMode::ReadOnly;
    let selection = match crate::runtime::resolve_selection(cfg, None) {
        Ok(s) => s,
        Err(_) => {
            return TaskResult {
                id: task.id.clone(),
                first_token_ms: None,
                wall_ms: 0,
                input_tokens: 0,
                output_tokens: 0,
                verified: None,
                errored: true,
            }
        }
    };
    let agent = crate::runtime::build_agent_with_fallback(cfg, &selection, mode).await;
    let mut agent = match agent {
        Ok(a) => a,
        Err(_) => {
            return TaskResult {
                id: task.id.clone(),
                first_token_ms: None,
                wall_ms: 0,
                input_tokens: 0,
                output_tokens: 0,
                verified: None,
                errored: true,
            }
        }
    };

    let start = Instant::now();
    let mut first_token_ms = None;
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut verified = None;
    let mut errored = false;

    let mut events = agent.run(task.prompt.clone());
    while let Some(event) = events.next().await {
        match &event {
            AgentEvent::TextDelta { .. } if first_token_ms.is_none() => {
                first_token_ms = Some(start.elapsed().as_millis());
            }
            AgentEvent::Usage { usage } => {
                input_tokens += usage.input_tokens as u64 + usage.cache_read_input_tokens as u64;
                output_tokens += usage.output_tokens as u64;
            }
            AgentEvent::Verification { passed, .. } => verified = Some(*passed),
            AgentEvent::Error { .. } => errored = true,
            _ => {}
        }
        if matches!(event, AgentEvent::Stop { .. }) {
            break;
        }
    }

    TaskResult {
        id: task.id.clone(),
        first_token_ms,
        wall_ms: start.elapsed().as_millis(),
        input_tokens,
        output_tokens,
        verified,
        errored,
    }
}

/// Parse a JSONL suite. Skips blank lines; errors on a malformed line.
pub fn parse_suite(text: &str) -> Result<Vec<BenchTask>> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(line).map_err(|e| anyhow::anyhow!("suite line {}: {e}", i + 1))?;
        let prompt = v
            .get("prompt")
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("suite line {}: missing `prompt`", i + 1))?
            .to_string();
        let id = v
            .get("id")
            .and_then(|p| p.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("task-{}", i + 1));
        out.push(BenchTask { id, prompt });
    }
    Ok(out)
}

fn builtin_suite() -> Vec<BenchTask> {
    [
        (
            "list-types",
            "List the public types in wingman-core. Be brief.",
        ),
        (
            "agent-loop",
            "In one paragraph, explain the agent loop in wingman-core.",
        ),
        (
            "find-tool",
            "Which built-in tool performs semantic search, and in which file?",
        ),
    ]
    .iter()
    .map(|(id, p)| BenchTask {
        id: id.to_string(),
        prompt: p.to_string(),
    })
    .collect()
}

/// Aggregate summary metrics over the results (medians/means as plain data).
pub fn summarize(results: &[TaskResult]) -> serde_json::Value {
    let n = results.len().max(1) as f64;
    let mean_wall = results.iter().map(|r| r.wall_ms as f64).sum::<f64>() / n;
    let ftts: Vec<u128> = results.iter().filter_map(|r| r.first_token_ms).collect();
    let mean_ftt = if ftts.is_empty() {
        None
    } else {
        Some(ftts.iter().map(|&x| x as f64).sum::<f64>() / ftts.len() as f64)
    };
    let mean_out = results.iter().map(|r| r.output_tokens as f64).sum::<f64>() / n;
    let verified_ran = results.iter().filter(|r| r.verified.is_some()).count();
    let verified_ok = results.iter().filter(|r| r.verified == Some(true)).count();
    let verified_rate = if verified_ran > 0 {
        Some(verified_ok as f64 / verified_ran as f64)
    } else {
        None
    };
    serde_json::json!({
        "task_count": results.len(),
        "mean_wall_ms": mean_wall,
        "mean_first_token_ms": mean_ftt,
        "mean_output_tokens": mean_out,
        "verified_done_rate": verified_rate,
        "errors": results.iter().filter(|r| r.errored).count(),
    })
}

fn result_to_json(r: &TaskResult) -> serde_json::Value {
    serde_json::json!({
        "id": r.id,
        "first_token_ms": r.first_token_ms,
        "wall_ms": r.wall_ms,
        "input_tokens": r.input_tokens,
        "output_tokens": r.output_tokens,
        "verified": r.verified,
        "errored": r.errored,
    })
}

fn print_table(results: &[TaskResult]) {
    println!(
        "{:<20} {:>10} {:>10} {:>10} {:>9} {:>9}",
        "task", "ftt(ms)", "wall(ms)", "out-tok", "verified", "error"
    );
    for r in results {
        println!(
            "{:<20} {:>10} {:>10} {:>10} {:>9} {:>9}",
            truncate(&r.id, 20),
            r.first_token_ms
                .map(|x| x.to_string())
                .unwrap_or_else(|| "—".into()),
            r.wall_ms,
            r.output_tokens,
            r.verified
                .map(|v| v.to_string())
                .unwrap_or_else(|| "—".into()),
            r.errored,
        );
    }
    let s = summarize(results);
    println!();
    println!("summary: {s}");
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonl_suite() {
        let text = "{\"id\":\"a\",\"prompt\":\"hi\"}\n\n{\"prompt\":\"yo\"}\n";
        let tasks = parse_suite(text).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "a");
        assert_eq!(tasks[1].id, "task-3"); // line index preserved
        assert_eq!(tasks[1].prompt, "yo");
    }

    #[test]
    fn malformed_line_errors() {
        assert!(parse_suite("{not json}").is_err());
        assert!(parse_suite("{\"id\":\"x\"}").is_err()); // missing prompt
    }

    #[test]
    fn summary_computes_rates() {
        let results = vec![
            TaskResult {
                id: "a".into(),
                first_token_ms: Some(100),
                wall_ms: 1000,
                input_tokens: 50,
                output_tokens: 20,
                verified: Some(true),
                errored: false,
            },
            TaskResult {
                id: "b".into(),
                first_token_ms: Some(300),
                wall_ms: 2000,
                input_tokens: 60,
                output_tokens: 40,
                verified: Some(false),
                errored: false,
            },
        ];
        let s = summarize(&results);
        assert_eq!(s["task_count"], 2);
        assert_eq!(s["mean_wall_ms"], 1500.0);
        assert_eq!(s["mean_first_token_ms"], 200.0);
        assert_eq!(s["verified_done_rate"], 0.5);
    }

    #[test]
    fn builtin_suite_is_nonempty() {
        assert!(!builtin_suite().is_empty());
    }
}
