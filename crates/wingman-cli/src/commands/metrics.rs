//! `wingman metrics` — the numbers `docs/DIFFERENTIATION.md` says to track,
//! for this repo, from what is already on disk.
//!
//! - **Time to first token**: per session, how long its first turn took from
//!   the prompt reaching the loop to the model's first output. Recorded by the
//!   loop on each `stop` record as `first_output_ms`.
//! - **Tokens per completed task**: every token the sessions spent (input,
//!   output, and both cache directions), divided by the turns that ended
//!   `end_turn`. Failed and abandoned turns stay in the numerator on purpose —
//!   what they burned is part of what the finished work cost.
//! - **Verified-done rate**: of the turns the verification gate ran on, the
//!   share whose last receipt was green; and of the sessions with any receipt,
//!   the share whose last one was.
//! - **Routing outcomes**: gate pass-rate per task class and model, from
//!   `learn.db` — the same rows `wingman router stats` prints.
//!
//! Sessions written before the loop recorded `first_output_ms`/`verified`
//! still count toward sessions, turns and tokens; they just contribute no
//! latency sample or receipt. Every figure reports its sample size, so a rate
//! over three turns does not read like one over three hundred.

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use serde::Serialize;
use wingman_config::ProjectPaths;
use wingman_session::{list_sessions, load_session, SessionRecord};

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Latency {
    pub median: Option<u64>,
    pub p90: Option<u64>,
    pub samples: usize,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Routing {
    pub task_class: String,
    pub model: String,
    pub passed: u32,
    pub total: u32,
    pub pass_rate: f32,
}

/// What `wingman metrics --json` prints and `GET .../metrics` returns.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Report {
    pub sessions: u32,
    pub turns: u32,
    pub completed_turns: u32,
    pub time_to_first_token_ms: Latency,
    pub total_tokens: u64,
    pub tokens_per_completed_task: Option<f64>,
    pub gated_turns: u32,
    pub verified_turns: u32,
    pub verified_done_rate: Option<f64>,
    pub sessions_with_receipt: u32,
    pub sessions_verified: u32,
    pub session_verified_done_rate: Option<f64>,
    pub routing: Vec<Routing>,
}

pub async fn run(json: bool) -> Result<ExitCode> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let paths = ProjectPaths::discover(&cwd);
    let report = collect(&paths);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(ExitCode::SUCCESS);
    }
    println!("Metrics for {}", paths.root.display());
    println!();
    for line in summary_lines(&report) {
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

/// The report for one project: its transcripts plus its routing rows.
pub fn collect(paths: &ProjectPaths) -> Report {
    // No stats db is not an error: routing outcomes accrue with use.
    let routing = wingman_learn::StatsStore::open_default()
        .and_then(|s| s.routing_summary(Some(&paths.root.to_string_lossy())))
        .unwrap_or_default();
    let mut report = scan(&paths.sessions_dir);
    report.routing = routing
        .into_iter()
        .map(|r| Routing {
            pass_rate: r.pass_rate(),
            task_class: r.task_class,
            model: r.model,
            passed: r.passed,
            total: r.total,
        })
        .collect();
    report
}

/// Walk every transcript under `dir`.
fn scan(dir: &Path) -> Report {
    let mut report = Report::default();
    let mut first_outputs = Vec::new();
    for path in list_sessions(dir) {
        // One unreadable transcript must not zero the report, same as the
        // cost timeline.
        let Ok(records) = load_session(&path) else {
            continue;
        };
        report.sessions += 1;
        let mut first_turn = true;
        let mut last_receipt = None;
        for record in &records {
            match record {
                SessionRecord::UsageDelta { usage, .. } => {
                    report.total_tokens += usage.input_tokens as u64
                        + usage.output_tokens as u64
                        + usage.cache_read_input_tokens as u64
                        + usage.cache_creation_input_tokens as u64;
                }
                SessionRecord::Stop {
                    reason,
                    first_output_ms,
                    verified,
                    ..
                } => {
                    report.turns += 1;
                    // Older loop builds wrote the reason JSON-quoted.
                    if reason.trim_matches('"') == "end_turn" {
                        report.completed_turns += 1;
                    }
                    if std::mem::take(&mut first_turn) {
                        first_outputs.extend(*first_output_ms);
                    }
                    if let Some(green) = verified {
                        report.gated_turns += 1;
                        report.verified_turns += u32::from(*green);
                        last_receipt = Some(*green);
                    }
                }
                _ => {}
            }
        }
        if let Some(green) = last_receipt {
            report.sessions_with_receipt += 1;
            report.sessions_verified += u32::from(green);
        }
    }

    first_outputs.sort_unstable();
    report.time_to_first_token_ms = Latency {
        median: percentile(&first_outputs, 0.5),
        p90: percentile(&first_outputs, 0.9),
        samples: first_outputs.len(),
    };
    report.tokens_per_completed_task = ratio(report.total_tokens as f64, report.completed_turns);
    report.verified_done_rate = ratio(report.verified_turns as f64, report.gated_turns);
    report.session_verified_done_rate = ratio(
        report.sessions_verified as f64,
        report.sessions_with_receipt,
    );
    report
}

/// Nearest-rank percentile of an ascending slice; `None` when empty.
pub fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

/// `n / d`, or `None` rather than a NaN or a confident zero when `d` is 0.
pub fn ratio(n: f64, d: u32) -> Option<f64> {
    (d > 0).then(|| n / d as f64)
}

/// The human summary, shared by `wingman metrics` and `wingman knows`.
pub fn summary_lines(r: &Report) -> Vec<String> {
    let pct = |x: Option<f64>| x.map_or("—".to_string(), |x| format!("{:.0}%", x * 100.0));
    let mut out = vec![format!(
        "sessions: {} · turns: {} ({} completed)",
        r.sessions, r.turns, r.completed_turns
    )];
    let t = &r.time_to_first_token_ms;
    out.push(match (t.median, t.p90) {
        (Some(m), Some(p)) => format!(
            "time to first token: median {m} ms · p90 {p} ms ({} sessions timed)",
            t.samples
        ),
        _ => "time to first token: — (no timed sessions yet)".into(),
    });
    out.push(match r.tokens_per_completed_task {
        Some(x) => format!(
            "tokens per completed task: {:.0} ({} tokens over {} completed turns)",
            x, r.total_tokens, r.completed_turns
        ),
        None => "tokens per completed task: — (no completed turns yet)".into(),
    });
    out.push(if r.gated_turns == 0 {
        "verified-done rate: — (the verification gate has not run yet)".into()
    } else {
        format!(
            "verified-done rate: {} of gated turns ({}/{}) · {} of sessions ({}/{})",
            pct(r.verified_done_rate),
            r.verified_turns,
            r.gated_turns,
            pct(r.session_verified_done_rate),
            r.sessions_verified,
            r.sessions_with_receipt
        )
    });
    if r.routing.is_empty() {
        out.push("routing outcomes: — (none recorded for this repo)".into());
    } else {
        out.push("routing outcomes (gate pass-rate):".into());
        for x in &r.routing {
            out.push(format!(
                "  {:<12} {:<40} {:>3}/{:<3} {:>4.0}%",
                x.task_class,
                x.model,
                x.passed,
                x.total,
                x.pass_rate * 100.0
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, lines: &[&str]) {
        std::fs::write(dir.join(name), lines.join("\n")).unwrap();
    }

    #[test]
    fn percentile_is_nearest_rank() {
        assert_eq!(percentile(&[], 0.5), None);
        assert_eq!(percentile(&[7], 0.9), Some(7));
        assert_eq!(percentile(&[1, 2, 3, 4], 0.5), Some(2));
        assert_eq!(percentile(&(1..=10).collect::<Vec<_>>(), 0.9), Some(9));
    }

    /// Two transcripts: one gated session that went red then green across
    /// two turns, and one old-format session with a quoted reason and no
    /// measurements. Every figure below is hand-computed from these lines.
    #[test]
    fn transcripts_reduce_to_the_three_metrics() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "20260901T100000000Z.jsonl",
            &[
                r#"{"kind":"session_start","ts":"t","model":"m","provider":"p","system_hash":null}"#,
                r#"{"kind":"usage_delta","ts":"t","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":20,"cache_creation_input_tokens":10}}"#,
                r#"{"kind":"stop","ts":"t","reason":"gate_failed","first_output_ms":800,"verified":false}"#,
                r#"{"kind":"usage_delta","ts":"t","usage":{"input_tokens":200,"output_tokens":20}}"#,
                r#"{"kind":"stop","ts":"t","reason":"end_turn","first_output_ms":300,"verified":true}"#,
            ],
        );
        write(
            dir.path(),
            "20260801T100000000Z.jsonl",
            &[
                r#"{"kind":"usage_delta","ts":"t","usage":{"input_tokens":400,"output_tokens":0}}"#,
                r#"{"kind":"stop","ts":"t","reason":"\"end_turn\""}"#,
            ],
        );
        // Unreadable: skipped, not fatal.
        write(dir.path(), "20260701T100000000Z.jsonl", &["{nope"]);

        let r = scan(dir.path());
        assert_eq!(r.sessions, 2);
        assert_eq!(r.turns, 3);
        assert_eq!(r.completed_turns, 2, "the quoted old reason still counts");
        // Only the first turn of a session is its time to first token.
        assert_eq!(
            r.time_to_first_token_ms,
            Latency {
                median: Some(800),
                p90: Some(800),
                samples: 1
            }
        );
        assert_eq!(r.total_tokens, 800);
        assert_eq!(r.tokens_per_completed_task, Some(400.0));
        assert_eq!((r.verified_turns, r.gated_turns), (1, 2));
        assert_eq!(r.verified_done_rate, Some(0.5));
        assert_eq!((r.sessions_verified, r.sessions_with_receipt), (1, 1));
        assert_eq!(r.session_verified_done_rate, Some(1.0));

        let text = summary_lines(&r).join("\n");
        assert!(text.contains("median 800 ms"), "{text}");
        assert!(text.contains("50% of gated turns (1/2)"), "{text}");
    }

    #[test]
    fn an_empty_repo_reports_absences_not_zeroes() {
        let dir = tempfile::tempdir().unwrap();
        let r = scan(&dir.path().join("missing"));
        assert_eq!(r.sessions, 0);
        assert_eq!(r.tokens_per_completed_task, None);
        assert_eq!(r.verified_done_rate, None);
        let text = summary_lines(&r).join("\n");
        assert!(text.contains("no completed turns yet"), "{text}");
        assert!(text.contains("none recorded"), "{text}");
    }
}
