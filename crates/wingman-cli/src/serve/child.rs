//! Run a `wingman` subcommand as a child process and stream its output.
//!
//! Why a child at all: `runtime::build_*` and most commands resolve the
//! project from `std::env::current_dir()`, which is process-wide. A daemon
//! serving several repos cannot set that per request without racing itself.
//! A child gets its own cwd, its own MCP servers and language servers, and
//! its own crash — a panicking turn ends one request instead of the daemon.
//! Pilot already spawns its workers exactly this way.
//!
//! The turn path relies on `--print --json`, whose stdout is one `AgentEvent`
//! per line. That maps onto SSE without a translation table: the event's
//! `type` becomes the SSE event name and the line becomes its data.

use std::path::Path;
use std::process::Stdio;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;
use wingman_config::PermissionMode;

use super::http::{self, Sse};

/// Build a `wingman` command rooted at `cwd`, with the permission ceiling in
/// the environment so nothing the child does can exceed what the API grants.
pub fn command(cwd: &Path, ceiling: PermissionMode) -> std::io::Result<Command> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.current_dir(cwd)
        .env("WINGMAN_PERMISSION_MODE", ceiling.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Kill the child when its handle drops — a client that disconnects
    // mid-turn should not leave an agent editing files unattended.
    cmd.kill_on_drop(true);
    Ok(cmd)
}

/// Run to completion, buffering both streams. For the short commands behind
/// the read routes, not for turns.
#[allow(dead_code)] // consumed by the read/admin routes (phase 4)
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

#[allow(dead_code)] // consumed by the read/admin routes (phase 4)
pub async fn run_to_completion(
    mut cmd: Command,
    timeout: std::time::Duration,
) -> std::io::Result<Output> {
    let child = cmd.spawn()?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok(Output {
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            code: out.status.code().unwrap_or(-1),
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "command timed out",
        )),
    }
}

/// Stream a child's NDJSON stdout to the client as SSE, one event per line.
///
/// Non-JSON stdout lines (a warning printed before the stream starts, say) are
/// forwarded as `log` events rather than dropped: silently swallowing a
/// child's complaint is how a remote debugging session turns into guesswork.
/// stderr is collected and, if the child fails, reported in the final event —
/// interleaving it into the stream would corrupt nothing, but a client
/// rendering a transcript does not want build noise inside the assistant's
/// reply.
pub async fn stream_events(
    mut cmd: Command,
    sock: &mut TcpStream,
    timeout: std::time::Duration,
) -> std::io::Result<()> {
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return http::write_err(sock, 500, &format!("spawning wingman: {e}")).await,
    };
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    // Drain stderr concurrently. Without this a child that writes more than a
    // pipe buffer of warnings blocks forever on the write, and the turn hangs
    // with no output — the worst possible failure for a remote client.
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });

    let mut sse = Sse::start(sock).await?;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut lines = BufReader::new(stdout).lines();

    loop {
        let next = tokio::time::timeout_at(deadline, lines.next_line()).await;
        match next {
            Err(_) => {
                let _ = child.kill().await;
                sse.send("error", &json!({ "message": "turn timed out" }))
                    .await?;
                return Ok(());
            }
            Ok(Err(e)) => {
                sse.send(
                    "error",
                    &json!({ "message": format!("reading child: {e}") }),
                )
                .await?;
                break;
            }
            Ok(Ok(None)) => break, // child closed stdout
            Ok(Ok(Some(line))) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let (kind, data) = classify(trimmed);
                sse.send(&kind, &data).await?;
            }
        }
    }

    let status = child.wait().await.ok();
    let code = status.and_then(|s| s.code()).unwrap_or(-1);
    let stderr_text = stderr_task.await.unwrap_or_default();
    sse.send(
        "end",
        &json!({
            "exit": code,
            // Only on failure: a successful turn's stderr is progress chatter.
            "stderr": (code != 0).then(|| tail(&stderr_text, 4000)),
        }),
    )
    .await?;
    Ok(())
}

/// Map one line of child stdout to an SSE `(event, data)` pair.
///
/// `--print --json` emits one `AgentEvent` per line, tagged with `type`, so
/// the event name comes straight off the wire — no translation table to keep
/// in sync as events are added. Anything that is not JSON is forwarded as a
/// `log` event rather than dropped: swallowing a child's complaint is how
/// remote debugging becomes guesswork.
fn classify(line: &str) -> (String, Value) {
    match serde_json::from_str::<Value>(line) {
        Ok(event) => {
            let kind = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("event")
                .to_string();
            (kind, event)
        }
        Err(_) => ("log".to_string(), json!({ "line": line })),
    }
}

/// Last `max` bytes of `s`, on a char boundary.
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let start = s.len() - max;
    let start = (start..s.len())
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(s.len());
    s[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_events_keep_their_type_as_the_sse_event_name() {
        let (kind, data) = classify("{\"type\":\"text_delta\",\"text\":\"hi\"}");
        assert_eq!(kind, "text_delta");
        assert_eq!(data["text"], "hi");

        let (kind, data) = classify("{\"type\":\"tool_start\",\"name\":\"read_file\"}");
        assert_eq!(kind, "tool_start");
        assert_eq!(data["name"], "read_file");
    }

    #[test]
    fn non_json_output_is_forwarded_not_dropped() {
        let (kind, data) = classify("wingman: falling back to openrouter");
        assert_eq!(kind, "log");
        assert_eq!(data["line"], "wingman: falling back to openrouter");
    }

    #[test]
    fn json_without_a_type_still_streams() {
        let (kind, _) = classify("{\"unexpected\":true}");
        assert_eq!(kind, "event");
    }

    #[test]
    fn tail_trims_to_a_char_boundary() {
        let s = "aé".repeat(4000);
        let t = tail(&s, 100);
        assert!(t.len() <= 100);
        // Round-trips as valid UTF-8 — i.e. we did not slice a code point.
        assert!(std::str::from_utf8(t.as_bytes()).is_ok());
        assert_eq!(tail("short", 100), "short");
    }
}
