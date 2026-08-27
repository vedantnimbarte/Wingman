//! `GET /v1/projects/{p}/cost/timeline` — spend per day, for one repo.
//!
//! `wingman cost` answers "what has this cost" from `~/.wingman/usage.json`,
//! which is a machine-wide running total with no timestamps in it. It cannot
//! answer "what did this cost *last week*", or "what does *this repo* cost" —
//! and those are the two questions anyone watching spend actually asks.
//!
//! Both answers are already on disk. Every session transcript carries
//! `usage_delta` records with a timestamp, and `session_start` names the model
//! they were billed at, so a day-by-day series is a walk over
//! `<project>/.wingman/sessions/*.jsonl` and one pricing lookup per delta. No
//! new file is written and nothing is recorded that was not already recorded.
//!
//! Two things this deliberately does not do:
//!
//! 1. **It does not read `usage.json`.** That file and this route measure
//!    different things — every project on the machine, versus the sessions in
//!    one repo — and they will disagree. The panel labels both rather than
//!    presenting one as the other.
//! 2. **It does not quietly drop what it cannot price.** A model missing from
//!    the pricing table is counted into `unpriced_turns`, so a total that is
//!    short says it is short.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use wingman_core::pricing::price_for;
use wingman_core::Usage;
use wingman_session::{list_sessions, load_session, SessionRecord};

use super::http::{self, Request};
use super::projects::Project;

/// Window when the request does not name one. A month is the span a spend
/// question is usually asked over; `?days=` widens or narrows it.
const DEFAULT_DAYS: usize = 30;

/// Ceiling on `?days=`. A year of daily buckets is 365 objects, which is a
/// chart; ten years is a download nobody plots.
const MAX_DAYS: usize = 365;

/// One day's spend, or one model's.
#[derive(Default, Clone, Copy)]
struct Bucket {
    usd: f64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    turns: u32,
}

impl Bucket {
    fn add(&mut self, usage: &Usage, usd: f64) {
        self.usd += usd;
        self.input += usage.input_tokens as u64;
        self.output += usage.output_tokens as u64;
        self.cache_read += usage.cache_read_input_tokens as u64;
        self.cache_write += usage.cache_creation_input_tokens as u64;
        self.turns += 1;
    }

    fn to_json(self, key: &str, value: Value) -> Value {
        json!({
            key: value,
            "usd": self.usd,
            "input_tokens": self.input,
            "output_tokens": self.output,
            "cache_read_tokens": self.cache_read,
            "cache_write_tokens": self.cache_write,
            "turns": self.turns,
        })
    }
}

/// Everything one pass over the transcripts produces.
#[derive(Default)]
struct Scan {
    days: BTreeMap<NaiveDate, Bucket>,
    models: BTreeMap<String, Bucket>,
    /// Turns whose model has no price. Reported, never guessed at.
    unpriced_turns: u32,
    sessions: u32,
}

impl Scan {
    /// Everything priced, across the whole history rather than one window.
    fn total(&self) -> f64 {
        self.models.values().map(|b| b.usd).sum()
    }
}

/// Transcripts carry two timestamp shapes: RFC-3339 from the interactive
/// path, and `epoch:<secs>` from `--print` (see `headless::chrono_rfc3339`).
/// Both are in the files on disk, so both parse here.
fn parse_ts(ts: &str) -> Option<DateTime<Utc>> {
    if let Some(secs) = ts.strip_prefix("epoch:") {
        return DateTime::from_timestamp(secs.trim().parse().ok()?, 0);
    }
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Walk every transcript in the project and price each usage delta.
///
/// The model is whatever the most recent `session_start` named. A resumed
/// session appends a second `session_start`, and `--model` on a later turn
/// changes what the rest of that log was billed at — reading it forward means
/// those turns are priced at the model that actually served them rather than
/// at the one the file opened with.
fn scan(project: &Project) -> Scan {
    let dir = project.root.join(".wingman").join("sessions");
    let mut out = Scan::default();

    for path in list_sessions(&dir) {
        let Ok(records) = load_session(&path) else {
            // One unreadable transcript must not empty the chart. It
            // contributes nothing and the rest of the repo still counts.
            continue;
        };
        out.sessions += 1;
        let mut model: Option<String> = None;

        for record in &records {
            match record {
                SessionRecord::SessionStart { model: m, .. } => model = Some(m.clone()),
                SessionRecord::UsageDelta { ts, usage } => {
                    let Some(key) = model.as_deref() else {
                        continue;
                    };
                    let Some(price) = price_for(key) else {
                        out.unpriced_turns += 1;
                        continue;
                    };
                    let usd = price.cost(usage);
                    out.models
                        .entry(key.to_string())
                        .or_default()
                        .add(usage, usd);
                    if let Some(at) = parse_ts(ts) {
                        out.days.entry(at.date_naive()).or_default().add(usage, usd);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// `GET /v1/projects/{p}/cost/timeline?days=N`
pub async fn get(project: &Project, req: &Request, sock: &mut TcpStream) -> std::io::Result<()> {
    let days = req
        .query_usize("days")
        .unwrap_or(DEFAULT_DAYS)
        .clamp(1, MAX_DAYS);
    let scan = scan(project);
    http::write_json(sock, 200, &report(&scan, days, Utc::now().date_naive())).await
}

/// Shape the scan into the window the request asked for.
///
/// Split from the handler so the bucketing and the gap fill can be tested
/// against a fixed "today" — a series that is one day wide on the machine
/// that runs the test and thirty on the machine that wrote it is not a test.
fn report(scan: &Scan, days: usize, today: NaiveDate) -> Value {
    // Gap-fill. A chart that plots three recorded days evenly spaced is
    // claiming they were consecutive; the zero days are data.
    let start = today - chrono::Duration::days(days as i64 - 1);
    let series: Vec<Value> = (0..days as i64)
        .map(|i| {
            let date = start + chrono::Duration::days(i);
            let bucket = scan.days.get(&date).copied().unwrap_or_default();
            bucket.to_json("date", json!(date.to_string()))
        })
        .collect();

    let window_usd: f64 = scan.days.range(start..=today).map(|(_, b)| b.usd).sum();

    let mut models: Vec<Value> = scan
        .models
        .iter()
        .map(|(name, b)| b.to_json("model", json!(name)))
        .collect();
    // Dearest first: the model to argue about is the one at the top.
    models.sort_by(|a, b| {
        b["usd"]
            .as_f64()
            .partial_cmp(&a["usd"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    json!({
        "days": series,
        "models": models,
        // The window the series covers, and the whole history behind it. The
        // panel needs both to say "nothing in the last 30 days, and the last
        // session was in February" instead of drawing an empty chart.
        "window_days": days,
        "window_usd": window_usd,
        "total_usd": scan.total(),
        "total_turns": scan.models.values().map(|b| b.turns).sum::<u32>(),
        "first_day": scan.days.keys().next().map(|d| d.to_string()),
        "last_day": scan.days.keys().next_back().map(|d| d.to_string()),
        "sessions": scan.sessions,
        "unpriced_turns": scan.unpriced_turns,
    })
}

/// Schema fragment, folded into `GET /v1/schema`.
pub fn schema() -> Vec<Value> {
    vec![json!({
        "method": "GET",
        "path": "/v1/projects/{project}/cost/timeline",
        "auth": true,
        "params": { "days": "window length, ending today (default 30, max 365)" },
        "about": "spend per day for this repo, priced from its session transcripts",
        "returns": "{days[], models[], window_usd, total_usd, first_day, last_day, unpriced_turns}",
    })]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn usage(input: u32, output: u32) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        }
    }

    /// Both timestamp shapes are in the transcripts on disk, so both have to
    /// land in the right bucket.
    #[test]
    fn both_timestamp_shapes_parse_to_the_same_day() {
        let rfc = parse_ts("2026-08-26T14:01:04.901040900+00:00").unwrap();
        let epoch = parse_ts("epoch:1787752864").unwrap();
        assert_eq!(rfc.date_naive(), day("2026-08-26"));
        assert_eq!(epoch.date_naive(), day("2026-08-26"));
        assert!(parse_ts("yesterday").is_none());
    }

    /// The gap fill is the honesty guarantee: days with no spend are zeroes in
    /// the series, not absences the chart would draw as adjacent.
    #[test]
    fn quiet_days_are_zeroes_rather_than_gaps() {
        let mut scan = Scan::default();
        scan.days
            .entry(day("2026-08-20"))
            .or_default()
            .add(&usage(1000, 100), 1.0);
        scan.days
            .entry(day("2026-08-22"))
            .or_default()
            .add(&usage(2000, 200), 2.0);

        let out = report(&scan, 5, day("2026-08-23"));
        let series = out["days"].as_array().unwrap();
        assert_eq!(series.len(), 5, "one entry per day in the window");
        assert_eq!(series[0]["date"], "2026-08-19");
        assert_eq!(series[0]["usd"], 0.0);
        assert_eq!(series[1]["usd"], 1.0);
        assert_eq!(series[2]["usd"], 0.0, "the quiet day is present and zero");
        assert_eq!(series[3]["usd"], 2.0);
        assert_eq!(out["window_usd"], 3.0);
    }

    /// A window that ends after the data still reports the history behind it,
    /// so the panel can say when the last session actually was.
    #[test]
    fn an_empty_window_still_reports_the_history() {
        let mut scan = Scan::default();
        scan.days
            .entry(day("2026-02-11"))
            .or_default()
            .add(&usage(1000, 100), 4.0);

        let out = report(&scan, 30, day("2026-08-23"));
        assert_eq!(out["window_usd"], 0.0);
        assert_eq!(out["first_day"], "2026-02-11");
        assert_eq!(out["last_day"], "2026-02-11");
        assert!(
            out["days"]
                .as_array()
                .unwrap()
                .iter()
                .all(|d| d["usd"] == 0.0),
            "nothing in the window"
        );
    }

    /// A turn priced at a model the table does not know is counted, not
    /// dropped — a total that is short has to say so.
    #[test]
    fn an_unpriceable_model_is_counted_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let sessions = root.join(".wingman").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("20260826T140044947Z.jsonl"),
            [
                r#"{"kind":"session_start","ts":"epoch:1787752844","model":"acme/whatever","provider":"x","system_hash":null}"#,
                r#"{"kind":"usage_delta","ts":"2026-08-26T14:01:04+00:00","usage":{"input_tokens":10,"output_tokens":2}}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        let scanned = scan(&Project {
            id: "repo".into(),
            root,
        });
        assert_eq!(scanned.unpriced_turns, 1);
        assert_eq!(scanned.total(), 0.0);
        assert_eq!(scanned.sessions, 1);
    }

    /// A real transcript, priced end to end.
    #[test]
    fn a_transcript_prices_into_its_day_and_its_model() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let sessions = root.join(".wingman").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("20260826T140044947Z.jsonl"),
            [
                r#"{"kind":"session_start","ts":"epoch:1787752844","model":"anthropic/claude-haiku-4-5","provider":"openrouter","system_hash":null}"#,
                r#"{"kind":"usage_delta","ts":"2026-08-26T14:01:04+00:00","usage":{"input_tokens":1000000,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        let scanned = scan(&Project {
            id: "repo".into(),
            root,
        });
        let expected = price_for("anthropic/claude-haiku-4-5")
            .unwrap()
            .input_per_mtok;
        assert!(
            (scanned.total() - expected).abs() < 1e-9,
            "one million input tokens is one input_per_mtok"
        );
        assert_eq!(scanned.days[&day("2026-08-26")].turns, 1);
        assert_eq!(
            scanned.models["anthropic/claude-haiku-4-5"].input,
            1_000_000
        );
    }
}
