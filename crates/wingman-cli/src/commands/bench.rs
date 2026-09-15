//! `wingman bench` — internal benchmark harness.
//!
//! Runs a suite of prompts headlessly and records the metrics that the
//! differentiation work is supposed to move: **time to first useful token**,
//! **tokens per completed task**, **wall time**, **verified-done rate** (did
//! the turn gate pass), and **routing outcomes** (which model served each
//! task, and how its tasks ended). Definitions match `wingman metrics`, so a
//! bench number and a day-to-day number are the same measurement. Needs a live
//! provider to run — it's a local measurement tool, not a CI job — but the
//! suite parsing, aggregation and report rendering are pure and unit-tested.
//!
//! Suite format: a JSONL file, one `{ "id": "...", "prompt": "..." }` per line.
//! With no `--suite`, a tiny built-in read-only suite runs.
//!
//! `--json` and `--markdown` are the publishable forms of one summary: for a
//! machine, and for a README or release note.

use anyhow::Result;
use futures::StreamExt;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;
use wingman_config::{global_config_path, Config, PermissionMode, ProjectPaths};
use wingman_core::{AgentEvent, AgentStop};

use super::metrics::{percentile, ratio};

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
#[derive(Debug, Clone, Default)]
pub struct TaskResult {
    pub id: String,
    /// The model that actually served the task, after any fallback. Empty
    /// when no agent could be built.
    pub model: String,
    pub first_token_ms: Option<u128>,
    pub wall_ms: u128,
    /// Input tokens, including cache reads and writes.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub verified: Option<bool>,
    /// The turn stopped `end_turn`: the model finished, rather than erroring,
    /// running out of budget, or giving up on a red gate.
    pub completed: bool,
    pub errored: bool,
}

pub async fn run(suite_path: Option<String>, json: bool, markdown: bool) -> Result<ExitCode> {
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
    } else if markdown {
        print!("{}", markdown_report(&results));
    } else {
        print_table(&results);
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_one(cfg: &Config, task: &BenchTask) -> TaskResult {
    let failed = |model: String| TaskResult {
        id: task.id.clone(),
        model,
        errored: true,
        ..Default::default()
    };
    // Read-only so a benchmark can't mutate the repo it runs in.
    let mode = PermissionMode::ReadOnly;
    let Ok(selection) = crate::runtime::resolve_selection(cfg, None) else {
        return failed(String::new());
    };
    let Ok(mut agent) = crate::runtime::build_agent_with_fallback(cfg, &selection, mode).await
    else {
        return failed(selection.model);
    };
    let model = agent.get_model().to_string();

    let start = Instant::now();
    let mut first_token_ms = None;
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut verified = None;
    let mut completed = false;
    let mut errored = false;

    let mut events = agent.run(task.prompt.clone());
    while let Some(event) = events.next().await {
        match &event {
            // Any output counts, as it does for the loop's own measurement:
            // a turn that opens with a tool call has started answering.
            AgentEvent::TextDelta { .. }
            | AgentEvent::ThinkingDelta { .. }
            | AgentEvent::ToolStart { .. }
                if first_token_ms.is_none() =>
            {
                first_token_ms = Some(start.elapsed().as_millis());
            }
            AgentEvent::Usage { usage } => {
                input_tokens += usage.input_tokens as u64
                    + usage.cache_read_input_tokens as u64
                    + usage.cache_creation_input_tokens as u64;
                output_tokens += usage.output_tokens as u64;
            }
            AgentEvent::Verification { passed, .. } => verified = Some(*passed),
            AgentEvent::Error { .. } => errored = true,
            AgentEvent::Stop { reason } => completed = *reason == AgentStop::EndTurn,
            _ => {}
        }
        if matches!(event, AgentEvent::Stop { .. }) {
            break;
        }
    }

    TaskResult {
        id: task.id.clone(),
        model,
        first_token_ms,
        wall_ms: start.elapsed().as_millis(),
        input_tokens,
        output_tokens,
        verified,
        completed,
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

/// Aggregate summary metrics over the results (plain data, sample sizes kept).
pub fn summarize(results: &[TaskResult]) -> serde_json::Value {
    let n = results.len().max(1) as f64;
    let mean_wall = results.iter().map(|r| r.wall_ms as f64).sum::<f64>() / n;
    let mut ftts: Vec<u64> = results
        .iter()
        .filter_map(|r| r.first_token_ms.map(|x| x as u64))
        .collect();
    ftts.sort_unstable();
    let mean_ftt = ratio(ftts.iter().map(|&x| x as f64).sum(), ftts.len() as u32);
    let mean_out = results.iter().map(|r| r.output_tokens as f64).sum::<f64>() / n;
    let tokens: u64 = results
        .iter()
        .map(|r| r.input_tokens + r.output_tokens)
        .sum();
    let completed = results.iter().filter(|r| r.completed).count() as u32;
    let verified_ran = results.iter().filter(|r| r.verified.is_some()).count() as u32;
    let verified_ok = results.iter().filter(|r| r.verified == Some(true)).count();

    // Per served model: what routing actually did on this suite.
    let mut routing: BTreeMap<&str, (u32, u32, u32, u32)> = BTreeMap::new();
    for r in results.iter().filter(|r| !r.model.is_empty()) {
        let row = routing.entry(&r.model).or_default();
        row.0 += 1;
        row.1 += u32::from(r.completed);
        row.2 += u32::from(r.verified.is_some());
        row.3 += u32::from(r.verified == Some(true));
    }
    let routing: Vec<serde_json::Value> = routing
        .into_iter()
        .map(|(model, (tasks, completed, gated, verified))| {
            serde_json::json!({
                "model": model,
                "tasks": tasks,
                "completed": completed,
                "gated": gated,
                "verified": verified,
            })
        })
        .collect();

    serde_json::json!({
        "task_count": results.len(),
        "completed": completed,
        "mean_wall_ms": mean_wall,
        "mean_first_token_ms": mean_ftt,
        "median_first_token_ms": percentile(&ftts, 0.5),
        "p90_first_token_ms": percentile(&ftts, 0.9),
        "mean_output_tokens": mean_out,
        "total_tokens": tokens,
        "tokens_per_completed_task": ratio(tokens as f64, completed),
        "verified_done_rate": ratio(verified_ok as f64, verified_ran),
        "routing": routing,
        "errors": results.iter().filter(|r| r.errored).count(),
    })
}

fn result_to_json(r: &TaskResult) -> serde_json::Value {
    serde_json::json!({
        "id": r.id,
        "model": r.model,
        "first_token_ms": r.first_token_ms,
        "wall_ms": r.wall_ms,
        "input_tokens": r.input_tokens,
        "output_tokens": r.output_tokens,
        "verified": r.verified,
        "completed": r.completed,
        "errored": r.errored,
    })
}

/// The summary as Markdown, ready to paste into a README or a release note.
pub fn markdown_report(results: &[TaskResult]) -> String {
    let s = summarize(results);
    let ms = |v: &serde_json::Value| v.as_u64().map_or("—".into(), |x| format!("{x} ms"));
    let pct = |v: &serde_json::Value| {
        v.as_f64()
            .map_or("—".into(), |x| format!("{:.0}%", x * 100.0))
    };
    let opt = |v: Option<bool>| v.map_or("—".into(), |b| b.to_string());

    let mut out = String::from("# Wingman bench\n\n");
    out.push_str(&format!(
        "{} tasks, {} completed, {} errored.\n\n",
        s["task_count"], s["completed"], s["errors"]
    ));
    out.push_str("| Metric | Value |\n|---|---|\n");
    out.push_str(&format!(
        "| Time to first token (median) | {} |\n",
        ms(&s["median_first_token_ms"])
    ));
    out.push_str(&format!(
        "| Time to first token (p90) | {} |\n",
        ms(&s["p90_first_token_ms"])
    ));
    out.push_str(&format!(
        "| Tokens per completed task | {} |\n",
        s["tokens_per_completed_task"]
            .as_f64()
            .map_or("—".into(), |x| format!("{x:.0}"))
    ));
    out.push_str(&format!(
        "| Verified-done rate | {} |\n",
        pct(&s["verified_done_rate"])
    ));
    out.push_str(&format!(
        "| Mean wall time | {:.0} ms |\n",
        s["mean_wall_ms"].as_f64().unwrap_or(0.0)
    ));

    out.push_str(
        "\n## Routing outcomes\n\n| Model | Tasks | Completed | Verified |\n|---|---|---|---|\n",
    );
    for row in s["routing"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "| {} | {} | {} | {}/{} |\n",
            row["model"].as_str().unwrap_or(""),
            row["tasks"],
            row["completed"],
            row["verified"],
            row["gated"]
        ));
    }

    out.push_str(
        "\n## Tasks\n\n| Task | Model | First token | Wall | Tokens in | Tokens out | Verified | Completed |\n|---|---|---|---|---|---|---|---|\n",
    );
    for r in results {
        out.push_str(&format!(
            "| {} | {} | {} | {} ms | {} | {} | {} | {} |\n",
            r.id,
            r.model,
            r.first_token_ms.map_or("—".into(), |x| format!("{x} ms")),
            r.wall_ms,
            r.input_tokens,
            r.output_tokens,
            opt(r.verified),
            r.completed
        ));
    }
    out
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

    fn results() -> Vec<TaskResult> {
        vec![
            TaskResult {
                id: "a".into(),
                model: "m1".into(),
                first_token_ms: Some(100),
                wall_ms: 1000,
                input_tokens: 50,
                output_tokens: 20,
                verified: Some(true),
                completed: true,
                errored: false,
            },
            TaskResult {
                id: "b".into(),
                model: "m1".into(),
                first_token_ms: Some(300),
                wall_ms: 2000,
                input_tokens: 60,
                output_tokens: 40,
                verified: Some(false),
                completed: false,
                errored: false,
            },
        ]
    }

    #[test]
    fn summary_computes_rates() {
        let s = summarize(&results());
        assert_eq!(s["task_count"], 2);
        assert_eq!(s["mean_wall_ms"], 1500.0);
        assert_eq!(s["mean_first_token_ms"], 200.0);
        assert_eq!(s["median_first_token_ms"], 100);
        assert_eq!(s["verified_done_rate"], 0.5);
        // All 170 tokens, over the one task that completed.
        assert_eq!(s["tokens_per_completed_task"], 170.0);
        assert_eq!(s["routing"][0]["model"], "m1");
        assert_eq!(s["routing"][0]["verified"], 1);
        assert_eq!(s["routing"][0]["gated"], 2);
    }

    #[test]
    fn markdown_report_carries_every_headline_number() {
        let md = markdown_report(&results());
        assert!(md.starts_with("# Wingman bench\n"));
        assert!(
            md.contains("| Time to first token (median) | 100 ms |"),
            "{md}"
        );
        assert!(md.contains("| Tokens per completed task | 170 |"), "{md}");
        assert!(md.contains("| Verified-done rate | 50% |"), "{md}");
        assert!(md.contains("| m1 | 2 | 1 | 1/2 |"), "{md}");
        assert!(md.contains("| b | m1 | 300 ms |"), "{md}");
    }

    #[test]
    fn builtin_suite_is_nonempty() {
        assert!(!builtin_suite().is_empty());
    }
}
