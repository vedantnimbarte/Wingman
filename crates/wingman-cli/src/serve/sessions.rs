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
use wingman_core::checkpoint;
use wingman_session::export::Format;
use wingman_session::{is_valid_session_id, list_sessions, load_session, SessionRecord};

use super::child;
use super::http::{self, Request};
use super::projects::Project;
use super::ServeState;

fn sessions_dir(project: &Project) -> std::path::PathBuf {
    project.root.join(".wingman").join("sessions")
}

/// Last-write time of a transcript, as Unix seconds.
///
/// Zero when the file is unreadable rather than an error: a session list that
/// refuses to render because one entry has odd permissions is worse than one
/// entry sorting to the bottom.
fn mtime_secs(path: &std::path::Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
            "mtime": mtime_secs(&path),
        }));
    }
    // Newest first. `list_sessions` returns directory order, which is neither
    // stable across platforms nor meaningful — and "which conversation was I
    // just in" is the only question a session list is ever opened to answer.
    out.sort_by(|a, b| b["mtime"].as_i64().cmp(&a["mtime"].as_i64()));
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

/// `GET /v1/projects/{p}/sessions/{id}/export?format=md|html|json[&download]`
///
/// The same report `wingman session export` prints, secrets redacted.
/// `download` adds `Content-Disposition: attachment`, which is how the panel
/// offers a file: a server-sent download needs no `data:` or blob link, which
/// a sandboxed page cannot open. The report quotes prompts and tool output, so
/// it is served with `nosniff` and a CSP that allows no script, and the HTML
/// form cannot run anything even when opened inline.
pub async fn export(
    project: &Project,
    id: &str,
    req: &Request,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    let format = match req.query_str("format").unwrap_or("md").parse::<Format>() {
        Ok(f) => f,
        Err(e) => return http::write_err(sock, 400, &e).await,
    };
    let Some(path) = wingman_session::session_path(&sessions_dir(project), id) else {
        return http::write_err(sock, 404, "no such session").await;
    };
    let export = match wingman_session::export::export_file(&path) {
        Ok(x) => x,
        Err(e) => return http::write_err(sock, 500, &format!("reading session: {e}")).await,
    };
    // `id` passed `session_path`'s validation, so it is safe in a header.
    let disposition = format!("attachment; filename=\"{id}.{}\"", format.extension());
    let mut headers = vec![
        ("X-Content-Type-Options", "nosniff"),
        (
            "Content-Security-Policy",
            "default-src 'none'; style-src 'unsafe-inline'",
        ),
    ];
    if req.query_bool("download") {
        headers.push(("Content-Disposition", disposition.as_str()));
    }
    http::write_raw(
        sock,
        200,
        format.content_type(),
        &headers,
        export.render(format).as_bytes(),
    )
    .await
}

/// `DELETE /v1/projects/{p}/sessions/{id}`
///
/// Removes the transcript *and* whatever was indexed from it. A finished turn
/// is embedded into the global session store for `recall_session`, so deleting
/// only the JSONL would leave the conversation retrievable by search — a
/// delete that does not delete.
pub async fn delete(project: &Project, id: &str, sock: &mut TcpStream) -> std::io::Result<()> {
    let Some(path) = wingman_session::session_path(&sessions_dir(project), id) else {
        return http::write_err(sock, 404, "no such session").await;
    };
    if let Err(e) = std::fs::remove_file(&path) {
        return http::write_err(sock, 500, &format!("deleting session: {e}")).await;
    }
    // Report the index outcome rather than swallowing it: "the transcript is
    // gone but recall may still find it" is something the caller should learn
    // from the response, not from a surprise later.
    let deindexed = match wingman_learn::session_index::forget_session(id) {
        Ok(found) => json!(found),
        Err(e) => {
            tracing::warn!("session {id} deleted but its index entry remains: {e}");
            json!({ "error": e.to_string() })
        }
    };
    http::write_json(sock, 200, &json!({ "deleted": id, "deindexed": deindexed })).await
}

/// This session's points on the project's rewind timeline, newest first, and
/// the session's transcript path and turns. `None` when the session has no log.
fn rewind_points(
    project: &Project,
    id: &str,
) -> Option<(
    std::path::PathBuf,
    Vec<checkpoint::Point>,
    Vec<wingman_session::TurnStart>,
)> {
    let path = wingman_session::session_path(&sessions_dir(project), id)?;
    let turns = wingman_session::turn_starts(&load_session(&path).unwrap_or_default());
    let points = checkpoint::timeline(&project.root)
        .into_iter()
        .filter(|p| p.session.as_deref() == Some(id))
        .collect();
    Some((path, points, turns))
}

/// `GET /v1/projects/{p}/sessions/{id}/rewind` — the edits each turn of this
/// session made, and its restores, newest first.
pub async fn rewind_timeline(
    project: &Project,
    id: &str,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    let Some((_, points, turns)) = rewind_points(project, id) else {
        return http::write_err(sock, 404, "no such session").await;
    };
    let points: Vec<Value> = points
        .iter()
        .map(|p| {
            json!({
                "seq": p.seq,
                "turn": p.turn,
                "prompt": p.turn.and_then(|t| turns.get(t)).map(|t| &t.prompt),
                "restore": p.restore,
                "ts": p.ts,
                "files": p.files,
            })
        })
        .collect();
    http::write_json(sock, 200, &json!({ "session_id": id, "points": points })).await
}

/// The point `seq` names, if it is one of this session's.
fn find_point(points: Vec<checkpoint::Point>, seq: &str) -> Option<checkpoint::Point> {
    let seq: u64 = seq.parse().ok()?;
    points.into_iter().find(|p| p.seq == seq)
}

/// `GET /v1/projects/{p}/sessions/{id}/rewind/{seq}` — what restoring to
/// before that point would change, file by file. Writes nothing.
///
/// The diffs are of files in the repo, which is where a `.env` lives, so they
/// pass through the tool-output secret redactor like everything else this
/// server hands a phone.
pub async fn rewind_preview(
    project: &Project,
    id: &str,
    seq: &str,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    let Some((_, points, _)) = rewind_points(project, id) else {
        return http::write_err(sock, 404, "no such session").await;
    };
    let Some(point) = find_point(points, seq) else {
        return http::write_err(sock, 404, "no such point in this session's timeline").await;
    };
    let changes = match checkpoint::preview(&project.root, point.seq) {
        Ok(c) => c,
        Err(e) => return http::write_err(sock, 409, &e).await,
    };
    let mut redacted = 0;
    let changes: Vec<Value> = changes
        .into_iter()
        .map(|c| {
            let (diff, n) = wingman_core::redact::redact_output_secrets(&c.diff);
            redacted += n;
            json!({
                "path": c.path,
                "exists_now": c.exists_now,
                "exists_after": c.exists_after,
                "diff": diff,
            })
        })
        .collect();
    http::write_json(
        sock,
        200,
        &json!({ "seq": point.seq, "changes": changes, "redacted": redacted }),
    )
    .await
}

/// Body for a restore.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RewindBody {
    /// Also fork the conversation to before this turn. Explicit, and off
    /// unless asked: the files and the conversation are separate choices.
    pub truncate: bool,
}

/// `POST /v1/projects/{p}/sessions/{id}/rewind/{seq}` — put the files back as
/// they were before that point.
///
/// The restore is itself checkpointed, so it lands on the timeline and can be
/// undone; no checkpoint is deleted. With `truncate`, the conversation is
/// forked to before the point's turn — a new session, the original untouched
/// — and its id comes back as `forked_session`.
pub async fn rewind(
    state: &Arc<ServeState>,
    project: &Project,
    id: &str,
    seq: &str,
    req: &Request,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    // Restoring rewrites files, so a server that may not write may not do it.
    if super::rank(state.ceiling) < super::rank(PermissionMode::AutoEdit) {
        return http::write_err(
            sock,
            403,
            &format!(
                "restoring files is a write, which this server's ceiling '{}' does not allow",
                state.ceiling
            ),
        )
        .await;
    }
    let body: RewindBody = match req.json::<Option<RewindBody>>() {
        Ok(b) => b.unwrap_or_default(),
        Err(e) => return http::write_err(sock, 400, &e).await,
    };
    let Some((path, points, turns)) = rewind_points(project, id) else {
        return http::write_err(sock, 404, "no such session").await;
    };
    let Some(point) = find_point(points, seq) else {
        return http::write_err(sock, 404, "no such point in this session's timeline").await;
    };
    // Checked before anything is written, so a truncate that cannot happen
    // does not leave the files restored and the conversation not.
    let truncate = match (body.truncate, point.turn) {
        (false, _) => None,
        (true, Some(turn)) if turn < turns.len() => Some(turn),
        (true, _) => {
            return http::write_err(
                sock,
                400,
                "only a turn in this session's transcript can truncate the conversation",
            )
            .await
        }
    };
    // A turn in flight is writing both the files and the transcript.
    if !try_claim(id).await {
        return http::write_err(sock, 409, "this session has a turn in flight").await;
    }
    let result = match checkpoint::restore_to(&project.root, point.seq, Some(id)) {
        Err(e) => http::write_err(sock, 409, &e).await,
        Ok(restored) => {
            let forked = match truncate {
                None => Ok(None),
                Some(turn) => wingman_session::fork_before_turn(&path, turn).await,
            };
            match forked {
                Ok(fork) => {
                    let fork = fork
                        .as_deref()
                        .and_then(|p| p.file_stem())
                        .map(|s| s.to_string_lossy());
                    http::write_json(
                        sock,
                        200,
                        &json!({ "restored": restored, "forked_session": fork }),
                    )
                    .await
                }
                Err(e) => {
                    http::write_err(
                        sock,
                        500,
                        &format!("files restored, but the conversation was not truncated: {e}"),
                    )
                    .await
                }
            }
        }
    };
    release(id).await;
    result
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
                "returns": "sessions with first prompt, model, turn count, mtime; newest first" }),
        json!({ "method": "GET", "path": "/v1/projects/{project}/sessions/{id}", "auth": true,
                "returns": "full transcript as SessionRecord[]" }),
        json!({ "method": "GET", "path": "/v1/projects/{project}/sessions/{id}/export", "auth": true,
                "query": { "format": "md | html | json (default md)", "download": "bool — send as an attachment" },
                "returns": "the session report: summary, files changed, receipts, cost, tool calls; secrets redacted" }),
        json!({ "method": "DELETE", "path": "/v1/projects/{project}/sessions/{id}", "auth": true }),
        json!({ "method": "GET", "path": "/v1/projects/{project}/sessions/{id}/rewind", "auth": true,
                "returns": "{session_id, points: [{seq, turn, prompt, restore, ts, files}]} — this session's edits per turn and its restores, newest first" }),
        json!({ "method": "GET", "path": "/v1/projects/{project}/sessions/{id}/rewind/{seq}", "auth": true,
                "returns": "{seq, changes: [{path, exists_now, exists_after, diff}], redacted} — what restoring to before the point would change; writes nothing" }),
        json!({ "method": "POST", "path": "/v1/projects/{project}/sessions/{id}/rewind/{seq}", "auth": true,
                "body": { "truncate": "bool — also fork the conversation to before this turn" },
                "returns": "{restored: string[], forked_session: string|null} — the restore is itself checkpointed; needs a ceiling of auto-edit or above" }),
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
