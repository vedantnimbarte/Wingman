//! `wingman acp` — speak the Agent Client Protocol over stdio.
//!
//! ACP is the cross-editor protocol for coding agents: one implementation
//! reaches Zed, JetBrains IDEs, Neovim, and Emacs, instead of hand-building a
//! plugin per editor. That is why this exists rather than a richer bespoke
//! VS Code panel — the ecosystem standardised the surface, so the leverage is
//! in speaking it.
//!
//! Transport is newline-delimited JSON-RPC 2.0 on stdin/stdout, the same shape
//! as `mcp-serve`. The agent side implements:
//!
//!   - `initialize`         — capability handshake
//!   - `session/new`        — start a session rooted at a cwd
//!   - `session/prompt`     — run one turn, streaming `session/update`
//!   - `session/cancel`     — notification; abandon the in-flight turn
//!
//! and sends `session/update` notifications to the client as the turn streams.
//!
//! Scope, stated plainly: this covers the turn loop — the part an editor needs
//! to show streaming output and tool activity. `session/request_permission`
//! and the client-side `fs/*` methods are not used yet; Wingman applies its own
//! permission model and touches the filesystem directly, which is correct but
//! means the editor cannot yet approve individual tool calls. Tracked in the
//! issue this shipped under.

use anyhow::Result;
use futures::StreamExt;
use serde_json::{json, Value};
use std::process::ExitCode;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use wingman_config::{Config, PermissionMode};
use wingman_core::{AgentEvent, AgentStop};

/// Protocol version implemented. Echoed back to the client when it asks for
/// this one; otherwise we still answer with ours so the client can decide.
const PROTOCOL_VERSION: u64 = 1;

pub async fn run(cfg: Config, mode: PermissionMode) -> Result<ExitCode> {
    eprintln!(
        "wingman acp: speaking Agent Client Protocol v{PROTOCOL_VERSION} over stdio (mode: {mode})"
    );

    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut line = String::new();

    // One session at a time is enough for an editor pane, and keeps the
    // cancellation story simple: there is exactly one turn to abandon.
    let mut session_id: Option<String> = None;
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));

    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break; // client closed the pipe
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            // Malformed input is the client's problem, but dropping it
            // silently makes debugging an editor integration miserable.
            eprintln!("wingman acp: ignoring unparseable line");
            continue;
        };

        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => {
                let reply = json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "agentInfo": {
                        "name": "wingman",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "agentCapabilities": {
                        // No auth: Wingman uses the credentials already in the
                        // user's keyring/config, so there is nothing for the
                        // editor to log into.
                        "loadSession": false,
                        "promptCapabilities": { "image": false, "audio": false },
                    },
                    "authMethods": [],
                });
                respond(&stdout, id, Ok(reply)).await?;
            }

            "session/new" => {
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(std::path::PathBuf::from);
                // Root the session where the editor says the workspace is, so
                // project containment and the semantic index line up with what
                // the user sees.
                if let Some(dir) = cwd.as_ref() {
                    if let Err(e) = std::env::set_current_dir(dir) {
                        respond(
                            &stdout,
                            id,
                            Err(format!("cannot enter {}: {e}", dir.display())),
                        )
                        .await?;
                        continue;
                    }
                }
                let sid = new_session_id();
                session_id = Some(sid.clone());
                respond(&stdout, id, Ok(json!({ "sessionId": sid }))).await?;
            }

            "session/prompt" => {
                let sid = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if session_id.as_deref() != Some(sid.as_str()) {
                    respond(&stdout, id, Err(format!("unknown session '{sid}'"))).await?;
                    continue;
                }
                let text = prompt_text(&params);
                if text.trim().is_empty() {
                    respond(&stdout, id, Err("empty prompt".into())).await?;
                    continue;
                }

                cancelled.store(false, std::sync::atomic::Ordering::SeqCst);
                let stop = run_turn(&cfg, mode, &sid, &text, &stdout, &cancelled).await;
                match stop {
                    Ok(reason) => {
                        respond(&stdout, id, Ok(json!({ "stopReason": reason }))).await?;
                    }
                    Err(e) => respond(&stdout, id, Err(e)).await?,
                }
            }

            // Notification: no id, no response.
            "session/cancel" => {
                cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
            }

            _ => {
                if id.is_some() {
                    respond(&stdout, id, Err(format!("unknown method '{method}'"))).await?;
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Run one turn, streaming `session/update` notifications as it goes.
async fn run_turn(
    cfg: &Config,
    mode: PermissionMode,
    session_id: &str,
    prompt: &str,
    stdout: &Arc<Mutex<tokio::io::Stdout>>,
    cancelled: &Arc<std::sync::atomic::AtomicBool>,
) -> std::result::Result<&'static str, String> {
    let selection = crate::runtime::resolve_selection(cfg, None).map_err(|e| e.to_string())?;
    let (mut agent, _registry) =
        crate::runtime::build_agent_registry_with_fallback(cfg, &selection, mode)
            .await
            .map_err(|e| e.to_string())?;

    let mut events = agent.run(prompt.to_string());
    let mut stop = "end_turn";

    while let Some(event) = events.next().await {
        if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok("cancelled");
        }
        match &event {
            AgentEvent::TextDelta { text } => {
                notify(
                    stdout,
                    session_id,
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": text },
                    }),
                )
                .await;
            }
            AgentEvent::ToolStart { id, name, .. } => {
                notify(
                    stdout,
                    session_id,
                    json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": id,
                        "title": name,
                        "kind": "other",
                        "status": "in_progress",
                    }),
                )
                .await;
            }
            AgentEvent::ToolResult { id, is_error, .. } => {
                notify(
                    stdout,
                    session_id,
                    json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": id,
                        "status": if *is_error { "failed" } else { "completed" },
                    }),
                )
                .await;
            }
            AgentEvent::Verification { passed, summary } => {
                // Surface the gate as a message chunk: an editor pane should
                // show that the build/tests ran and what they said.
                notify(
                    stdout,
                    session_id,
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": {
                            "type": "text",
                            "text": format!(
                                "\n[verify {}] {summary}\n",
                                if *passed { "passed" } else { "FAILED" }
                            ),
                        },
                    }),
                )
                .await;
            }
            AgentEvent::Stop { reason } => {
                stop = match reason {
                    AgentStop::EndTurn => "end_turn",
                    AgentStop::MaxTurns => "max_turn_requests",
                    AgentStop::MaxTokens => "max_tokens",
                    // ACP has no "the build is red" reason; `refusal` is the
                    // closest honest mapping — the agent declined to call it
                    // done rather than finishing cleanly.
                    AgentStop::GateFailed => "refusal",
                    AgentStop::Error => "refusal",
                };
                break;
            }
            AgentEvent::Error { message } => {
                return Err(message.clone());
            }
            _ => {}
        }
    }
    Ok(stop)
}

/// Flatten ACP's ContentBlock array into the plain text the agent loop takes.
/// Non-text blocks are skipped rather than rendered as placeholders: an editor
/// that sends an image to an agent advertising `image: false` is better served
/// by the text it did send than by a confusing stub.
fn prompt_text(params: &Value) -> String {
    let Some(blocks) = params.get("prompt").and_then(Value::as_array) else {
        return String::new();
    };
    blocks
        .iter()
        .filter_map(|b| match b.get("type").and_then(Value::as_str) {
            Some("text") => b.get("text").and_then(Value::as_str).map(str::to_string),
            Some("resource") => b
                .get("resource")
                .and_then(|r| r.get("text"))
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn new_session_id() -> String {
    // pid + millis alone collides when two sessions start in the same
    // millisecond, which a client reconnecting immediately will do. The
    // counter makes it unique regardless of clock resolution.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(
        "wingman-{}-{}-{n}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    )
}

/// Write a JSON-RPC response (or error) for `id`.
async fn respond(
    stdout: &Arc<Mutex<tokio::io::Stdout>>,
    id: Option<Value>,
    result: std::result::Result<Value, String>,
) -> Result<()> {
    let Some(id) = id else {
        return Ok(()); // notification: nothing to answer
    };
    let msg = match result {
        Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
        Err(e) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32603, "message": e },
        }),
    };
    write_line(stdout, &msg).await
}

/// Send a `session/update` notification.
async fn notify(stdout: &Arc<Mutex<tokio::io::Stdout>>, session_id: &str, update: Value) {
    let msg = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": { "sessionId": session_id, "update": update },
    });
    let _ = write_line(stdout, &msg).await;
}

async fn write_line(stdout: &Arc<Mutex<tokio::io::Stdout>>, msg: &Value) -> Result<()> {
    let mut out = stdout.lock().await;
    out.write_all(msg.to_string().as_bytes()).await?;
    out.write_all(b"\n").await?;
    out.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_text_flattens_text_blocks() {
        let params = json!({
            "prompt": [
                { "type": "text", "text": "fix the flaky test" },
                { "type": "text", "text": "in auth.rs" },
            ]
        });
        assert_eq!(prompt_text(&params), "fix the flaky test\nin auth.rs");
    }

    #[test]
    fn prompt_text_reads_resource_blocks() {
        let params = json!({
            "prompt": [
                { "type": "text", "text": "explain" },
                { "type": "resource", "resource": { "uri": "file:///a.rs", "text": "fn main() {}" } },
            ]
        });
        assert_eq!(prompt_text(&params), "explain\nfn main() {}");
    }

    /// An unknown block type must not become a placeholder in the prompt —
    /// the model would treat the stub as content.
    #[test]
    fn prompt_text_skips_unsupported_blocks() {
        let params = json!({
            "prompt": [
                { "type": "image", "data": "…" },
                { "type": "text", "text": "only this" },
            ]
        });
        assert_eq!(prompt_text(&params), "only this");
    }

    #[test]
    fn prompt_text_handles_missing_or_empty() {
        assert_eq!(prompt_text(&json!({})), "");
        assert_eq!(prompt_text(&json!({ "prompt": [] })), "");
    }

    #[test]
    fn session_ids_are_unique_per_call() {
        assert_ne!(new_session_id(), new_session_id());
    }
}
