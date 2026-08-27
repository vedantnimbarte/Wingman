//! Pilot routes: read a run, watch it live, steer it.
//!
//! Everything here is filesystem work. A run is already an append-only
//! `tasks.jsonl` plus an atomically-rewritten `state.json`, and control is
//! already "append a JSON line to `control.jsonl`" that the orchestrator's
//! watchdog picks up. So the API never needs to reach into a live
//! orchestrator process — it reads the same files `wingman pilot watch` reads
//! and writes the same file `wingman pilot approve` writes. A run started
//! from a laptop is fully steerable from a phone, and neither knows about the
//! other.
//!
//! The one thing this layer adds over `control::append` is a state check.
//! The control file is deliberately lenient — an unrecognised or
//! inapplicable command is skipped rather than wedging the reader — which is
//! right for a file an operator might hand-edit, but wrong for an API: a
//! client that gets `200` for approving a run that was never gated has been
//! told something false. So these routes load the state first and answer
//! `409` when the command cannot apply.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use wingman_autonomous::control::ControlCommand;
use wingman_autonomous::dashboard;
use wingman_autonomous::model::{RunState, RunStatus, TaskStatus};

use super::http::{self, Request, Sse, SSE_KEEPALIVE};
use super::projects::Project;
use super::ServeState;

/// How often the live stream looks for newly appended events. The
/// orchestrator writes on task transitions, not continuously, so this is
/// about latency-to-the-phone rather than throughput.
const STREAM_POLL: Duration = Duration::from_millis(500);

fn run_dir(project: &Project, run_id: &str) -> PathBuf {
    project
        .root
        .join(".wingman")
        .join("autonomous")
        .join(run_id)
}

/// Reject a run id that could climb out of the runs directory. Ids are minted
/// as `YYYY-MM-DD-HHMM-<rand6>`, so this costs nothing legitimate.
fn valid_run_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

async fn load_or_404(
    project: &Project,
    run_id: &str,
    sock: &mut TcpStream,
) -> std::io::Result<Option<(PathBuf, RunState)>> {
    if !valid_run_id(run_id) {
        http::write_err(sock, 400, "malformed run id").await?;
        return Ok(None);
    }
    let dir = run_dir(project, run_id);
    match dashboard::load_state(&dir) {
        Ok(state) => Ok(Some((dir, state))),
        Err(_) => {
            http::write_err(sock, 404, "no such run").await?;
            Ok(None)
        }
    }
}

/// `GET /v1/projects/{p}/pilot/runs`
pub async fn list_runs(project: &Project, sock: &mut TcpStream) -> std::io::Result<()> {
    let runs = match dashboard::list_runs(&project.root) {
        Ok(r) => r,
        Err(e) => return http::write_err(sock, 500, &format!("listing runs: {e}")).await,
    };
    // `RunSummary` is not `Serialize` (it is a TUI picker type), so the wire
    // shape is built here — which is the right place for it anyway: this is
    // the API contract, and it should not move because a TUI field was
    // renamed.
    let list: Vec<Value> = runs
        .iter()
        .map(|r| {
            json!({
                "run_id": r.run_id,
                "goal": r.goal,
                "status": r.status,
                "done": r.done,
                "total": r.total,
                "terminal": r.is_terminal(),
            })
        })
        .collect();
    http::write_json(sock, 200, &json!({ "runs": list })).await
}

/// `GET /v1/projects/{p}/pilot/runs/{run}`
pub async fn get_run(project: &Project, run_id: &str, sock: &mut TcpStream) -> std::io::Result<()> {
    let Some((_, state)) = load_or_404(project, run_id, sock).await? else {
        return Ok(());
    };
    let body = serde_json::to_value(&state).unwrap_or_else(|_| json!({}));
    http::write_json(sock, 200, &body).await
}

/// `GET /v1/projects/{p}/pilot/runs/{run}/events?tail=n`
pub async fn get_events(
    project: &Project,
    run_id: &str,
    req: &Request,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    let Some((dir, _)) = load_or_404(project, run_id, sock).await? else {
        return Ok(());
    };
    let tail = req.query_usize("tail").unwrap_or(50).min(1000);
    match dashboard::tail_events(&dir, tail) {
        Ok(events) => http::write_json(sock, 200, &json!({ "events": events })).await,
        Err(e) => http::write_err(sock, 500, &format!("reading events: {e}")).await,
    }
}

/// `GET /v1/projects/{p}/pilot/runs/{run}/dashboard?width=n`
///
/// The same ASCII dashboard `pilot watch` draws. A phone that can render a
/// monospace block gets the whole run at a glance for one request and no
/// client-side code.
pub async fn get_dashboard(
    project: &Project,
    run_id: &str,
    req: &Request,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    let Some((dir, state)) = load_or_404(project, run_id, sock).await? else {
        return Ok(());
    };
    let recent = dashboard::tail_events(&dir, 20).unwrap_or_default();
    let width = req.query_usize("width").unwrap_or(100).clamp(40, 400);
    let text = dashboard::render_dashboard(&state, &recent).to_ascii_width(width);
    http::write_text(sock, 200, &text).await
}

/// How many trailing lines a log request returns by default, and at most.
///
/// A pilot log is one run's stdout, so the interesting part is almost always
/// the end — the plan it settled on, and whatever it died of.
const LOG_TAIL: usize = 500;
const LOG_TAIL_MAX: usize = 5000;

/// `GET /v1/projects/{p}/pilot/runs/{run}/log?tail=n`
///
/// The orchestrator's own stdout, which `tasks.jsonl` does not contain and
/// nothing else serves. `events` reports what the run *did* — a task moved,
/// a tool ran — while this is what it *said*: the plan it estimated, why it
/// asked for approval or did not, and the error it exited on. A run whose
/// events stop mid-task and whose status is `failed` is explained here and
/// nowhere else.
///
/// Returns the tail with both counts, so a client can say "the last 500 of
/// 2,341 lines" instead of presenting a truncated log as a whole one.
pub async fn get_log(
    project: &Project,
    run_id: &str,
    req: &Request,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    let Some((dir, _)) = load_or_404(project, run_id, sock).await? else {
        return Ok(());
    };
    let path = dir.join("pilot.log");
    // ponytail: reads the whole log to take its tail. A run's stdout is
    // bounded by the run, and every one on disk here is well under a
    // megabyte; seek from the end if a long-running pilot ever makes this
    // measurable.
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        // A run that has not written a line yet is not an error — a run that
        // was planned and never started has no log, and the caller wants an
        // empty log rather than a 500 it has to special-case.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return http::write_err(sock, 500, &format!("reading log: {e}")).await,
    };

    let tail = req
        .query_usize("tail")
        .unwrap_or(LOG_TAIL)
        .min(LOG_TAIL_MAX);
    let lines: Vec<&str> = text.lines().collect();
    let shown = lines.len().min(tail);
    let body = lines[lines.len() - shown..].join("\n");

    http::write_json(
        sock,
        200,
        &json!({
            "text": body,
            "total_lines": lines.len(),
            "shown_lines": shown,
        }),
    )
    .await
}

/// `GET /v1/projects/{p}/pilot/runs/{run}/stream?tail=n`
///
/// Replays the last `tail` events, then streams new ones as they are
/// appended, and closes with an `end` event once the run reaches a terminal
/// status. Tailing by byte offset (rather than re-reading and diffing) means
/// a long run does not get more expensive to watch as its log grows.
pub async fn stream(
    project: &Project,
    run_id: &str,
    req: &Request,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    let Some((dir, _)) = load_or_404(project, run_id, sock).await? else {
        return Ok(());
    };
    let log = dir.join("tasks.jsonl");
    let tail = req.query_usize("tail").unwrap_or(20).min(1000);

    let mut offset = start_offset(&log, tail);
    let mut sse = Sse::start(sock).await?;
    let mut last_write = Instant::now();

    loop {
        let (lines, new_offset) = read_from(&log, offset);
        offset = new_offset;
        for line in lines {
            let Some(event) = serde_json::from_str::<Value>(&line).ok() else {
                continue; // a partially-flushed line; it will be complete next poll
            };
            let kind = event
                .get("ev")
                .and_then(Value::as_str)
                .unwrap_or("event")
                .to_string();
            sse.send(&kind, &event).await?;
            last_write = Instant::now();
        }

        // Close the stream when the run is over, so a client knows it has
        // seen everything rather than waiting on a socket that will never
        // produce another byte.
        if let Ok(state) = dashboard::load_state(&dir) {
            if is_terminal(state.status) {
                sse.send("end", &json!({ "status": state.status })).await?;
                return Ok(());
            }
        }

        if last_write.elapsed() >= SSE_KEEPALIVE {
            sse.keepalive().await?;
            last_write = Instant::now();
        }
        tokio::time::sleep(STREAM_POLL).await;
    }
}

fn is_terminal(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Done | RunStatus::Failed | RunStatus::Aborted
    )
}

/// Byte offset at which to begin streaming: the start of the last `tail`
/// lines, or end-of-file when `tail` is 0.
fn start_offset(log: &Path, tail: usize) -> u64 {
    let Ok(bytes) = std::fs::read(log) else {
        return 0;
    };
    if tail == 0 {
        return bytes.len() as u64;
    }
    let newlines: Vec<usize> = bytes
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
        .map(|(i, _)| i)
        .collect();
    if newlines.len() <= tail {
        return 0;
    }
    (newlines[newlines.len() - tail - 1] + 1) as u64
}

/// Read complete lines from `offset`, returning them and the offset just past
/// the last complete line. A trailing partial line is left for the next poll,
/// so a half-flushed event is never parsed.
fn read_from(log: &Path, offset: u64) -> (Vec<String>, u64) {
    let Ok(bytes) = std::fs::read(log) else {
        return (Vec::new(), offset);
    };
    let start = (offset as usize).min(bytes.len());
    let slice = &bytes[start..];
    let last_newline = slice.iter().rposition(|b| *b == b'\n');
    let Some(end) = last_newline else {
        return (Vec::new(), offset);
    };
    let complete = &slice[..=end];
    let lines: Vec<String> = String::from_utf8_lossy(complete)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect();
    (lines, (start + end + 1) as u64)
}

/// Body accepted by the control routes. Every field optional so an empty POST
/// is valid for the commands that take no argument.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ControlBody {
    pub task: Option<String>,
}

/// `POST …/pilot/runs/{run}/{approve|veto|abort|retry}`
pub async fn control(
    project: &Project,
    run_id: &str,
    action: &str,
    req: &Request,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    let Some((dir, state)) = load_or_404(project, run_id, sock).await? else {
        return Ok(());
    };
    let body: ControlBody = match req.json::<Option<ControlBody>>() {
        Ok(b) => b.unwrap_or_default(),
        Err(e) => return http::write_err(sock, 400, &e).await,
    };

    let cmd = match action {
        "approve" | "veto" => {
            if state.status != RunStatus::AwaitingApproval {
                return http::write_err(
                    sock,
                    409,
                    &format!(
                        "run is '{}', not awaiting approval — nothing to {action}",
                        status_name(state.status)
                    ),
                )
                .await;
            }
            if action == "approve" {
                ControlCommand::Approve
            } else {
                ControlCommand::Veto
            }
        }
        "abort" => {
            if is_terminal(state.status) {
                return http::write_err(
                    sock,
                    409,
                    &format!("run already finished ('{}')", status_name(state.status)),
                )
                .await;
            }
            match body.task {
                Some(id) => {
                    if !state.tasks.iter().any(|t| t.id == id) {
                        return http::write_err(sock, 404, "no such task in this run").await;
                    }
                    ControlCommand::AbortTask { id }
                }
                None => ControlCommand::AbortRun,
            }
        }
        "retry" => {
            let Some(id) = body.task else {
                return http::write_err(sock, 400, "retry needs {\"task\": \"<id>\"}").await;
            };
            let Some(task) = state.tasks.iter().find(|t| t.id == id) else {
                return http::write_err(sock, 404, "no such task in this run").await;
            };
            // Re-queueing a task that is running or already done would either
            // duplicate work or race the worker holding its worktree.
            if !matches!(task.status, TaskStatus::Failed | TaskStatus::Blocked) {
                return http::write_err(sock, 409, "only failed or blocked tasks can be retried")
                    .await;
            }
            ControlCommand::RetryTask { id }
        }
        _ => return http::write_err(sock, 404, "unknown control action").await,
    };

    match wingman_autonomous::control::append(&dir, &cmd) {
        Ok(()) => {
            http::write_json(
                sock,
                202,
                &json!({ "accepted": action, "run_id": run_id, "command": cmd }),
            )
            .await
        }
        Err(e) => http::write_err(sock, 500, &format!("writing control command: {e}")).await,
    }
}

fn status_name(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Planning => "planning",
        RunStatus::AwaitingApproval => "awaiting_approval",
        RunStatus::Running => "running",
        RunStatus::Merging => "merging",
        RunStatus::Done => "done",
        RunStatus::Failed => "failed",
        RunStatus::Aborted => "aborted",
    }
}

/// Body for `POST …/pilot/runs`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct StartBody {
    pub goal: String,
    /// Skip the interactive plan gate. Without it a gated run parks in
    /// `awaiting_approval` until something approves it — which over the API
    /// is the point: plan from a phone, approve from a phone.
    pub yes: bool,
    pub plan_only: bool,
    pub model: Option<String>,
    pub tier: Option<String>,
    pub max_usd: Option<f64>,
}

/// `POST /v1/projects/{p}/pilot/runs` — start a run detached and return its id.
///
/// Spawns `wingman pilot run -d`, which mints the run id, re-execs itself in
/// the background, and prints the id. Reusing the detach path means the run
/// outlives the request, the daemon, and a dropped connection — a phone that
/// goes to sleep does not kill the fleet.
pub async fn start_run(
    state: &Arc<ServeState>,
    project: &Project,
    req: &Request,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    let body: StartBody = match req.json::<Option<StartBody>>() {
        Ok(b) => b.unwrap_or_default(),
        Err(e) => return http::write_err(sock, 400, &e).await,
    };
    if body.goal.trim().is_empty() {
        return http::write_err(sock, 400, "a run needs a non-empty \"goal\"").await;
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return http::write_err(sock, 500, &format!("resolving executable: {e}")).await,
    };
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("pilot")
        .arg("run")
        .arg("-d")
        .arg(&body.goal)
        .current_dir(&project.root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if body.yes {
        cmd.arg("--yes");
    }
    if body.plan_only {
        cmd.arg("--plan-only");
    }
    if let Some(m) = &body.model {
        cmd.arg("--model").arg(m);
    }
    if let Some(t) = &body.tier {
        cmd.arg("--tier").arg(t);
    }
    if let Some(u) = body.max_usd {
        cmd.arg("--max-usd").arg(u.to_string());
    }
    // The run inherits the server's ceiling, so a request cannot get a fleet
    // of workers with more authority than the API itself grants.
    cmd.env("WINGMAN_PERMISSION_MODE", state.ceiling.to_string());

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => return http::write_err(sock, 500, &format!("spawning pilot: {e}")).await,
    };
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    match parse_run_id(&stdout) {
        Some(run_id) => {
            http::write_json(sock, 202, &json!({ "run_id": run_id, "detached": true })).await
        }
        None => {
            http::write_json(
                sock,
                500,
                &json!({
                    "error": "pilot did not report a run id",
                    "stdout": stdout,
                    "stderr": stderr,
                }),
            )
            .await
        }
    }
}

/// Pull the run id out of `[pilot] run <id> detached (pid …)`.
fn parse_run_id(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("[pilot] run ")?;
        let id = rest.split_whitespace().next()?;
        valid_run_id(id).then(|| id.to_string())
    })
}

/// Body for `POST …/pilot/goals`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct GoalBody {
    pub text: String,
    pub author: Option<String>,
}

/// `POST /v1/projects/{p}/pilot/goals` — queue work for the discovery daemon.
///
/// Writes an intake file, the same transport-agnostic drop-box the Slack and
/// email adapters use. The author is recorded but earns no trust here: the
/// daemon decides trust from `[pilot.daemon].trusted_authors`, and a request
/// that could name itself into an allowlisted identity would turn a token
/// into unattended auto-run.
pub async fn add_goal(
    state: &Arc<ServeState>,
    project: &Project,
    req: &Request,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    let body: GoalBody = match req.json::<Option<GoalBody>>() {
        Ok(b) => b.unwrap_or_default(),
        Err(e) => return http::write_err(sock, 400, &e).await,
    };
    if body.text.trim().is_empty() {
        return http::write_err(sock, 400, "a goal needs non-empty \"text\"").await;
    }
    let dir = project.root.join(&state.cfg.pilot.daemon.intake_dir);
    match crate::commands::pilot_intake::write_intake(&dir, body.author.as_deref(), &body.text) {
        Ok(path) => {
            http::write_json(
                sock,
                202,
                &json!({
                    "queued": true,
                    "file": path.file_name().map(|n| n.to_string_lossy().to_string()),
                }),
            )
            .await
        }
        Err(e) => http::write_err(sock, 500, &format!("writing intake file: {e}")).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ids_cannot_traverse() {
        assert!(valid_run_id("2026-08-18-1042-a3f1zz"));
        assert!(!valid_run_id("../../../etc"));
        assert!(!valid_run_id("a/b"));
        assert!(!valid_run_id(""));
    }

    #[test]
    fn parses_the_detached_run_id() {
        let out = "[pilot] run 2026-08-18-1042-a3f1zz detached (pid 4242, log: x)\n\
                   [pilot] watch:  wingman pilot watch 2026-08-18-1042-a3f1zz\n";
        assert_eq!(parse_run_id(out).as_deref(), Some("2026-08-18-1042-a3f1zz"));
        assert_eq!(parse_run_id("nothing useful here"), None);
    }

    #[test]
    fn tail_offset_picks_the_last_n_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("tasks.jsonl");
        std::fs::write(&log, "a\nb\nc\nd\n").unwrap();
        // tail=0 starts at EOF: only new events.
        assert_eq!(start_offset(&log, 0), 8);
        // tail=2 starts at the beginning of "c".
        assert_eq!(start_offset(&log, 2), 4);
        // More requested than exist: from the top.
        assert_eq!(start_offset(&log, 99), 0);
    }

    #[test]
    fn partial_lines_are_left_for_the_next_poll() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("tasks.jsonl");
        std::fs::write(&log, "{\"ev\":\"a\"}\n{\"ev\":\"b\"}\n{\"ev\":\"hal").unwrap();
        let (lines, offset) = read_from(&log, 0);
        assert_eq!(lines.len(), 2);
        // The offset stops after the last complete line, so the half-written
        // event is re-read whole once the writer finishes it.
        assert_eq!(offset, 22);
        let (more, _) = read_from(&log, offset);
        assert!(more.is_empty());
    }
}
