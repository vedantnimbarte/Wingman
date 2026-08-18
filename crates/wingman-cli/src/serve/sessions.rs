//! Sessions and turns: hold a conversation with the agent over HTTP.
//!
//! A session is not an in-memory object with a timeout. It is the same
//! `<project>/.wingman/sessions/<id>.jsonl` the TUI and `--print` write, so a
//! conversation started from a phone survives a daemon restart, shows up in
//! `wingman session list`, and can be resumed from the terminal. The server
//! keeps no conversation state at all — the transcript on disk *is* the
//! state, which is why "close the laptop, continue from the phone" works
//! without a sync protocol.
//!
//! Each turn spawns `wingman --print --json --resume <id>`, which replays the
//! transcript into the agent and appends this turn to it.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use wingman_config::PermissionMode;
use wingman_session::{is_valid_session_id, list_sessions, load_session, SessionRecord};

use super::child;
use super::http::{self, Request};
use super::projects::Project;
use super::ServeState;

fn sessions_dir(project: &Project) -> std::path::PathBuf {
    project.root.join(".wingman").join("sessions")
}

/// `POST /v1/projects/{p}/sessions` — mint an id.
///
/// No file is written yet: an empty session is just an id, and creating a log
/// for a conversation that never happens would litter `session list` with
/// blanks. The first turn creates it.
pub async fn create(sock: &mut TcpStream) -> std::io::Result<()> {
    let id = wingman_session::new_session_id();
    http::write_json(sock, 201, &json!({ "session_id": id })).await
}

/// `GET /v1/projects/{p}/sessions`
pub async fn list(project: &Project, sock: &mut TcpStream) -> std::io::Result<()> {
    let dir = sessions_dir(project);
    let mut out = Vec::new();
    for path in list_sessions(&dir) {
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let records = load_session(&path).unwrap_or_default();
        let first_prompt = records.iter().find_map(|r| match r {
            SessionRecord::User { text, .. } => Some(text.clone()),
            _ => None,
        });
        let (model, provider) = records
            .iter()
            .find_map(|r| match r {
                SessionRecord::SessionStart {
                    model, provider, ..
                } => Some((Some(model.clone()), Some(provider.clone()))),
                _ => None,
            })
            .unwrap_or((None, None));
        let turns = records
            .iter()
            .filter(|r| matches!(r, SessionRecord::User { .. }))
            .count();
        out.push(json!({
            "session_id": id,
            "first_prompt": first_prompt,
            "model": model,
            "provider": provider,
            "turns": turns,
        }));
    }
    http::write_json(sock, 200, &json!({ "sessions": out })).await
}

/// `GET /v1/projects/{p}/sessions/{id}` — the full transcript.
pub async fn get(project: &Project, id: &str, sock: &mut TcpStream) -> std::io::Result<()> {
    let Some(path) = wingman_session::session_path(&sessions_dir(project), id) else {
        return http::write_err(sock, 404, "no such session").await;
    };
    match load_session(&path) {
        Ok(records) => {
            http::write_json(sock, 200, &json!({ "session_id": id, "records": records })).await
        }
        Err(e) => http::write_err(sock, 500, &format!("reading session: {e}")).await,
    }
}

/// `DELETE /v1/projects/{p}/sessions/{id}`
pub async fn delete(project: &Project, id: &str, sock: &mut TcpStream) -> std::io::Result<()> {
    let Some(path) = wingman_session::session_path(&sessions_dir(project), id) else {
        return http::write_err(sock, 404, "no such session").await;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => http::write_json(sock, 200, &json!({ "deleted": id })).await,
        Err(e) => http::write_err(sock, 500, &format!("deleting session: {e}")).await,
    }
}

/// Body for a turn.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TurnBody {
    pub prompt: String,
    /// Requested permission mode. Clamped by the server's ceiling; asking for
    /// more is a 403 rather than a silent downgrade.
    pub mode: Option<String>,
    pub model: Option<String>,
}

/// Sessions with a turn in flight. A second turn on the same session would
/// have the child replay a transcript the first turn is still appending to,
/// so the two would interleave into one incoherent history.
static IN_FLIGHT: Mutex<Option<HashSet<String>>> = Mutex::const_new(None);

async fn try_claim(id: &str) -> bool {
    let mut guard = IN_FLIGHT.lock().await;
    guard
        .get_or_insert_with(HashSet::new)
        .insert(id.to_string())
}

async fn release(id: &str) {
    let mut guard = IN_FLIGHT.lock().await;
    if let Some(set) = guard.as_mut() {
        set.remove(id);
    }
}

/// `POST /v1/projects/{p}/sessions/{id}/turns` — run a turn in a session.
/// `POST /v1/projects/{p}/turns` — run a one-shot turn (`id` = `None`).
pub async fn turn(
    state: &Arc<ServeState>,
    project: &Project,
    id: Option<&str>,
    req: &Request,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    let body: TurnBody = match req.json::<Option<TurnBody>>() {
        Ok(b) => b.unwrap_or_default(),
        Err(e) => return http::write_err(sock, 400, &e).await,
    };
    if body.prompt.trim().is_empty() {
        return http::write_err(sock, 400, "a turn needs a non-empty \"prompt\"").await;
    }
    if let Some(id) = id {
        if !is_valid_session_id(id) {
            return http::write_err(sock, 400, "malformed session id").await;
        }
    }

    // Resolve the mode before doing anything expensive, so a request that
    // over-asks is refused rather than quietly served with less authority.
    let requested = match body.mode.as_deref() {
        Some(m) => match m.parse::<PermissionMode>() {
            Ok(m) => Some(m),
            Err(e) => return http::write_err(sock, 400, &e).await,
        },
        None => None,
    };
    let mode = match state.effective_mode(requested) {
        Ok(m) => m,
        Err(asked) => {
            return http::write_err(
                sock,
                403,
                &format!(
                    "requested mode '{asked}' exceeds this server's ceiling '{}'",
                    state.ceiling
                ),
            )
            .await
        }
    };

    // Bound total concurrent agent work across every project. `try_acquire`
    // rather than waiting: a client holding an SSE connection open in a queue
    // it cannot see is worse than being told to retry.
    let Ok(_permit) = state.turns.try_acquire() else {
        return http::write_err(
            sock,
            429,
            "all turn slots are busy ([serve].max_concurrent_turns)",
        )
        .await;
    };

    if let Some(id) = id {
        if !try_claim(id).await {
            return http::write_err(sock, 409, "this session already has a turn in flight").await;
        }
    }

    let mut cmd = match child::command(&project.root, mode) {
        Ok(c) => c,
        Err(e) => {
            if let Some(id) = id {
                release(id).await;
            }
            return http::write_err(sock, 500, &format!("resolving executable: {e}")).await;
        }
    };
    cmd.arg("--print")
        .arg(&body.prompt)
        .arg("--json")
        .arg("--mode")
        .arg(mode.to_string());
    if let Some(m) = &body.model {
        cmd.arg("--model").arg(m);
    }
    if let Some(id) = id {
        // `--resume` on a session with no log yet would fail, so only pass it
        // once the transcript exists; `--session-id` alone names the log the
        // first turn creates.
        if wingman_session::session_path(&sessions_dir(project), id).is_some() {
            cmd.arg("--resume").arg(id);
        } else {
            cmd.arg("--session-id").arg(id);
        }
    }

    let timeout = Duration::from_secs(state.cfg.serve.request_timeout_secs.max(1));
    let result = child::stream_events(cmd, sock, timeout).await;
    if let Some(id) = id {
        release(id).await;
    }
    result
}

/// Schema fragment for these routes, folded into `GET /v1/schema`.
pub fn schema() -> Vec<Value> {
    vec![
        json!({ "method": "POST", "path": "/v1/projects/{project}/sessions", "auth": true,
                "returns": "{session_id} — mints an id; the first turn creates the log" }),
        json!({ "method": "GET", "path": "/v1/projects/{project}/sessions", "auth": true,
                "returns": "sessions with first prompt, model, turn count" }),
        json!({ "method": "GET", "path": "/v1/projects/{project}/sessions/{id}", "auth": true,
                "returns": "full transcript as SessionRecord[]" }),
        json!({ "method": "DELETE", "path": "/v1/projects/{project}/sessions/{id}", "auth": true }),
        json!({ "method": "POST", "path": "/v1/projects/{project}/sessions/{id}/turns", "auth": true,
                "body": { "prompt": "string", "mode": "string?", "model": "string?" },
                "returns": "text/event-stream of agent events, then 'end'" }),
        json!({ "method": "POST", "path": "/v1/projects/{project}/turns", "auth": true,
                "body": { "prompt": "string", "mode": "string?", "model": "string?" },
                "returns": "same, without session continuity" }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_session_can_only_have_one_turn_in_flight() {
        let id = "20260818T104200000Z-test";
        assert!(try_claim(id).await);
        assert!(!try_claim(id).await, "second claim must be refused");
        release(id).await;
        assert!(try_claim(id).await, "released ids can be claimed again");
        release(id).await;
    }

    #[tokio::test]
    async fn different_sessions_do_not_block_each_other() {
        assert!(try_claim("a").await);
        assert!(try_claim("b").await);
        release("a").await;
        release("b").await;
    }
}
