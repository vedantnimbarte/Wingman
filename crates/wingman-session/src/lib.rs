//! Append-only JSONL session log.
//!
//! One file per session under `<project>/.wingman/sessions/<timestamp>.jsonl`.
//! Each record is a single line of JSON. Records are typed via a `kind` field
//! so a reader can interleave user prompts, assistant text/tool calls,
//! results, and usage updates.
//!
//! Future M4 work (`/resume`) reads the same file back; the format must
//! remain backwards-compatible — only additive fields.

use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
pub mod store;

pub use store::{FileSessionStore, MemorySessionStore, SessionStore};

use wingman_core::{AgentEvent, ContentBlock, ContextFact, Message, Role, Usage};

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error(
        "'{0}' is not a valid session id (letters, digits, '.', '-' and '_' only, 1-128 chars)"
    )]
    BadId(String),
}

/// A session id names a file, so it must not be able to name a *path*.
/// Rejecting the separators outright is cheaper to reason about than
/// canonicalising afterwards, and costs nothing legitimate: ids are minted
/// as timestamps.
pub fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Mint a session id in the same timestamp form [`SessionLog::create`] uses,
/// so ids from every entry point sort together and read alike.
pub fn new_session_id() -> String {
    Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionRecord {
    SessionStart {
        ts: String,
        model: String,
        provider: String,
        system_hash: Option<String>,
    },
    User {
        ts: String,
        text: String,
    },
    Assistant {
        ts: String,
        blocks: Vec<ContentBlock>,
    },
    ToolResult {
        ts: String,
        id: String,
        /// What the tool produced, in full — the audit trail, and what the
        /// user was shown.
        output: String,
        /// The bounded form actually sent to the model, when it differed
        /// (truncated, and carrying a spill locator). Absent means the model
        /// saw `output` verbatim.
        ///
        /// Recorded separately so the log can answer both "what did the tool
        /// say" and "what did the model receive" — logging only `output` made
        /// a resumed session richer than the one it replaced.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_output: Option<String>,
        is_error: bool,
    },
    /// Compaction folded the oldest `replaced` messages into `text`.
    Recap {
        ts: String,
        replaced: usize,
        text: String,
    },
    /// An earlier tool result was shrunk in place to reclaim context.
    ToolResultPruned {
        ts: String,
        id: String,
        content: String,
    },
    /// Text spliced onto the system prompt for one turn. Not part of message
    /// history, so it does not replay — but it changed the request, and a
    /// reader asking why the agent did something needs to see it.
    InjectedContext {
        ts: String,
        text: String,
    },
    UsageDelta {
        ts: String,
        usage: Usage,
    },
    Stop {
        ts: String,
        reason: String,
    },
}

/// Copy `src` to a freshly-named session file under `dest_dir`, optionally
/// truncating to the first `take` records (`None` = full copy).
///
/// Returns the path of the new session file. Useful for `wingman session
/// fork`: the new file is `/resume`-able and the original is untouched.
pub async fn fork_session(
    src: &Path,
    dest_dir: &Path,
    take: Option<usize>,
) -> Result<PathBuf, SessionError> {
    tokio::fs::create_dir_all(dest_dir).await?;
    let body = tokio::fs::read_to_string(src).await?;
    let mut out = String::new();
    let total = match take {
        Some(n) => n,
        None => usize::MAX,
    };
    for (i, line) in body.lines().enumerate() {
        if i >= total {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    let ts = Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string();
    let dest = dest_dir.join(format!("{ts}-fork.jsonl"));
    tokio::fs::write(&dest, out).await?;
    Ok(dest)
}

pub struct SessionLog {
    path: PathBuf,
    file: tokio::fs::File,
}

impl SessionLog {
    /// Open a new session file under `sessions_dir`. The directory is created
    /// if missing.
    pub async fn create(sessions_dir: &Path) -> Result<Self, SessionError> {
        tokio::fs::create_dir_all(sessions_dir).await?;
        let ts = Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string();
        let path = sessions_dir.join(format!("{ts}.jsonl"));
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(Self { path, file })
    }

    /// Open the session named `id` under `sessions_dir`, appending if it
    /// already exists. This is how a conversation continues across processes:
    /// `--print --session-id <id>` writes into the same log the previous turn
    /// wrote, so `--resume` can rebuild the history.
    ///
    /// `id` is validated rather than trusted. It arrives from a command-line
    /// flag and, with the HTTP API, from a request — a `..` in it would place
    /// the log outside the project.
    pub async fn open_named(sessions_dir: &Path, id: &str) -> Result<Self, SessionError> {
        if !is_valid_session_id(id) {
            return Err(SessionError::BadId(id.to_string()));
        }
        tokio::fs::create_dir_all(sessions_dir).await?;
        let path = sessions_dir.join(format!("{id}.jsonl"));
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(Self { path, file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The session id: the log's filename without `.jsonl`.
    pub fn id(&self) -> String {
        self.path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    pub async fn write(&mut self, record: SessionRecord) -> Result<(), SessionError> {
        let line = serde_json::to_string(&record)?;
        self.file.write_all(line.as_bytes()).await?;
        self.file.write_all(b"\n").await?;
        Ok(())
    }

    pub async fn record_message(&mut self, msg: &Message) -> Result<(), SessionError> {
        let ts = now();
        match msg.role {
            Role::User => {
                // A user message may be either a fresh prompt or a bundle of
                // tool_result blocks; serialize tool_result blocks separately
                // and only emit a `User { text }` record for free text.
                for b in &msg.content {
                    match b {
                        ContentBlock::Text { text } => {
                            self.write(SessionRecord::User {
                                ts: ts.clone(),
                                text: text.clone(),
                            })
                            .await?;
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            self.write(SessionRecord::ToolResult {
                                ts: ts.clone(),
                                id: tool_use_id.clone(),
                                output: content.clone(),
                                model_output: None,
                                is_error: *is_error,
                            })
                            .await?;
                        }
                        ContentBlock::ToolUse { .. } | ContentBlock::Thinking { .. } => {
                            /* should not appear from user */
                        }
                        ContentBlock::Image { media_type, .. } => {
                            // Record image attachments as a brief note in the session log.
                            self.write(SessionRecord::User {
                                ts: ts.clone(),
                                text: format!("[image: {media_type}]"),
                            })
                            .await?;
                        }
                    }
                }
            }
            Role::Assistant => {
                self.write(SessionRecord::Assistant {
                    ts,
                    blocks: msg.content.clone(),
                })
                .await?;
            }
        }
        Ok(())
    }

    /// Write one [`ContextFact`] — the loop's record of what the model saw.
    ///
    /// This is the single writer the session log is supposed to have. It
    /// replaced per-surface hand-rolled logging, which is why the TUI's logs
    /// used to contain no tool calls at all.
    pub async fn record_fact(&mut self, fact: &ContextFact) -> Result<(), SessionError> {
        let ts = now();
        match fact {
            ContextFact::UserMessage { text } => {
                self.write(SessionRecord::User {
                    ts,
                    text: text.clone(),
                })
                .await
            }
            ContextFact::AssistantMessage { blocks } => {
                self.write(SessionRecord::Assistant {
                    ts,
                    blocks: blocks.clone(),
                })
                .await
            }
            ContextFact::ToolResult {
                id,
                full,
                model_view,
                is_error,
            } => {
                self.write(SessionRecord::ToolResult {
                    ts,
                    id: id.clone(),
                    output: full.clone(),
                    model_output: model_view.clone(),
                    is_error: *is_error,
                })
                .await
            }
            ContextFact::Compacted { replaced, recap } => {
                self.write(SessionRecord::Recap {
                    ts,
                    replaced: *replaced,
                    text: recap.clone(),
                })
                .await
            }
            ContextFact::ToolResultPruned { id, content } => {
                self.write(SessionRecord::ToolResultPruned {
                    ts,
                    id: id.clone(),
                    content: content.clone(),
                })
                .await
            }
            ContextFact::SystemInjected { text } => {
                self.write(SessionRecord::InjectedContext {
                    ts,
                    text: text.clone(),
                })
                .await
            }
            ContextFact::Usage { usage } => {
                self.write(SessionRecord::UsageDelta { ts, usage: *usage })
                    .await
            }
            ContextFact::Stop { reason } => {
                self.write(SessionRecord::Stop {
                    ts,
                    reason: reason.clone(),
                })
                .await
            }
        }
    }

    pub async fn record_agent_event(&mut self, event: &AgentEvent) -> Result<(), SessionError> {
        match event {
            AgentEvent::Usage { usage } => {
                self.write(SessionRecord::UsageDelta {
                    ts: now(),
                    usage: *usage,
                })
                .await
            }
            AgentEvent::Stop { reason } => {
                self.write(SessionRecord::Stop {
                    ts: now(),
                    reason: serde_json::to_value(reason)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_else(|| "unknown".into()),
                })
                .await
            }
            _ => Ok(()), // Other events are derived from the messages we log separately.
        }
    }
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

/// List all session files under `sessions_dir`, sorted by filename (newest first,
/// because filenames are ISO timestamps that sort lexicographically).
pub fn list_sessions(sessions_dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(sessions_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .collect(),
        Err(_) => Vec::new(),
    };
    // Sort descending so the newest session comes first.
    paths.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    paths
}

/// Path of the session named `id` under `sessions_dir`, if it exists.
pub fn session_path(sessions_dir: &Path, id: &str) -> Option<PathBuf> {
    if !is_valid_session_id(id) {
        return None;
    }
    let path = sessions_dir.join(format!("{id}.jsonl"));
    path.is_file().then_some(path)
}

/// Load all records from a session JSONL file.
pub fn load_session(path: &Path) -> Result<Vec<SessionRecord>, SessionError> {
    let text = std::fs::read_to_string(path)?;
    let mut records = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record: SessionRecord = serde_json::from_str(trimmed)?;
        records.push(record);
    }
    Ok(records)
}

/// Reconstruct a conversation history from session records, suitable for
/// passing to `AgentLoop::with_history`.
///
/// - `SessionRecord::User`       → `Message::user_text(text)`
/// - `SessionRecord::Assistant`  → `Message { role: Assistant, content: blocks }`
/// - `SessionRecord::ToolResult` → accumulated and flushed as `Message::tool_results(...)`
///   when the next non-`ToolResult` record (or end of slice) is reached
/// - All other records           → ignored
pub fn records_to_messages(records: &[SessionRecord]) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::new();
    let mut pending_tool_results: Vec<ContentBlock> = Vec::new();

    let flush_tool_results = |pending: &mut Vec<ContentBlock>, messages: &mut Vec<Message>| {
        if !pending.is_empty() {
            messages.push(Message::tool_results(std::mem::take(pending)));
        }
    };

    for record in records {
        match record {
            SessionRecord::ToolResult {
                id,
                output,
                model_output,
                is_error,
                ..
            } => {
                pending_tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    // The bounded form when there was one: reconstructing the
                    // conversation means reconstructing what the model saw,
                    // not the richer thing the tool said.
                    content: model_output.clone().unwrap_or_else(|| output.clone()),
                    is_error: *is_error,
                });
            }
            SessionRecord::User { text, .. } => {
                flush_tool_results(&mut pending_tool_results, &mut messages);
                messages.push(Message::user_text(text.clone()));
            }
            SessionRecord::Assistant { blocks, .. } => {
                flush_tool_results(&mut pending_tool_results, &mut messages);
                messages.push(Message {
                    role: Role::Assistant,
                    content: blocks.clone(),
                });
            }
            SessionRecord::Recap { replaced, text, .. } => {
                // Compaction replaced the oldest `replaced` messages with a
                // recap. Replay it the same way, or the reconstruction is a
                // conversation the model never had — a longer one.
                flush_tool_results(&mut pending_tool_results, &mut messages);
                let drop_to = (*replaced).min(messages.len());
                messages.drain(0..drop_to);
                messages.insert(0, Message::user_text(text.clone()));
            }
            SessionRecord::ToolResultPruned { id, content, .. } => {
                // Applied in place, wherever that result ended up.
                for message in messages.iter_mut() {
                    for block in message.content.iter_mut() {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content: existing,
                            ..
                        } = block
                        {
                            if tool_use_id == id {
                                *existing = content.clone();
                            }
                        }
                    }
                }
                for block in pending_tool_results.iter_mut() {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content: existing,
                        ..
                    } = block
                    {
                        if tool_use_id == id {
                            *existing = content.clone();
                        }
                    }
                }
            }
            _ => {
                // SessionStart, UsageDelta, Stop, InjectedContext — none of
                // these are message history. InjectedContext rode the system
                // prompt, which is rebuilt per turn rather than replayed.
            }
        }
    }

    // Flush any trailing tool results.
    flush_tool_results(&mut pending_tool_results, &mut messages);

    messages
}

/// Extract `(provider, model)` from the first `SessionStart` record in the slice.
pub fn session_meta(records: &[SessionRecord]) -> Option<(String, String)> {
    for record in records {
        if let SessionRecord::SessionStart {
            provider, model, ..
        } = record
        {
            return Some((provider.clone(), model.clone()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use wingman_core::AgentStop;

    /// Write a message and a couple of records, then read the file back and
    /// confirm the log round-trips through JSONL without loss.
    #[tokio::test]
    async fn write_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::create(dir.path()).await.unwrap();
        let path = log.path().to_path_buf();

        log.write(SessionRecord::SessionStart {
            ts: "t0".into(),
            model: "claude".into(),
            provider: "anthropic".into(),
            system_hash: None,
        })
        .await
        .unwrap();
        log.record_message(&Message::user_text("hello"))
            .await
            .unwrap();
        drop(log); // flush by closing the handle

        let records = load_session(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(
            session_meta(&records),
            Some(("anthropic".into(), "claude".into()))
        );
        assert!(matches!(&records[1], SessionRecord::User { text, .. } if text == "hello"));
    }

    /// The stop reason is stored as the bare variant name, matching what the
    /// ContextFact::Stop writer records — not a JSON-quoted `"\"end_turn\""`.
    #[tokio::test]
    async fn stop_reason_is_written_unquoted() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::create(dir.path()).await.unwrap();
        let path = log.path().to_path_buf();
        log.record_agent_event(&AgentEvent::Stop {
            reason: AgentStop::EndTurn,
        })
        .await
        .unwrap();
        drop(log);

        let records = load_session(&path).unwrap();
        assert!(matches!(&records[0], SessionRecord::Stop { reason, .. } if reason == "end_turn"));
    }

    /// load_session ignores blank lines rather than erroring on them.
    #[test]
    fn load_skips_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(
            &path,
            "{\"kind\":\"user\",\"ts\":\"t\",\"text\":\"hi\"}\n\n  \n",
        )
        .unwrap();
        assert_eq!(load_session(&path).unwrap().len(), 1);
    }

    /// Consecutive ToolResult records must collapse into a single user message
    /// of tool_result blocks, and that message must land *before* the following
    /// user prompt — the ordering AgentLoop::with_history depends on.
    #[test]
    fn tool_results_accumulate_then_flush_before_next_prompt() {
        let records = vec![
            SessionRecord::User {
                ts: "t".into(),
                text: "q1".into(),
            },
            SessionRecord::ToolResult {
                ts: "t".into(),
                id: "a".into(),
                output: "ra".into(),
                model_output: None,
                is_error: false,
            },
            SessionRecord::ToolResult {
                ts: "t".into(),
                id: "b".into(),
                output: "rb".into(),
                model_output: None,
                is_error: true,
            },
            SessionRecord::User {
                ts: "t".into(),
                text: "q2".into(),
            },
        ];
        let msgs = records_to_messages(&records);
        // q1, [tool_results a+b], q2
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1].content.len(), 2); // both tool results in one message
        assert!(
            matches!(&msgs[1].content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "a")
        );
        assert!(matches!(&msgs[2].content[0], ContentBlock::Text { text } if text == "q2"));
    }

    /// Trailing tool results (no following prompt) are still flushed.
    #[test]
    fn trailing_tool_results_are_flushed() {
        let records = vec![SessionRecord::ToolResult {
            ts: "t".into(),
            id: "x".into(),
            output: "out".into(),
            model_output: None,
            is_error: false,
        }];
        assert_eq!(records_to_messages(&records).len(), 1);
    }

    /// list_sessions returns only .jsonl files, newest (highest timestamp) first.
    #[test]
    fn list_sessions_is_newest_first_and_jsonl_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("20240101T000000000Z.jsonl"), "").unwrap();
        std::fs::write(dir.path().join("20240202T000000000Z.jsonl"), "").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "").unwrap();
        let got = list_sessions(dir.path());
        assert_eq!(got.len(), 2);
        assert!(got[0]
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("20240202"));
    }

    /// fork_session with a `take` limit copies only the first N records.
    #[tokio::test]
    async fn fork_truncates_to_take() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        std::fs::write(&src, "l1\nl2\nl3\n").unwrap();
        let forked = fork_session(&src, dir.path(), Some(2)).await.unwrap();
        assert_eq!(std::fs::read_to_string(&forked).unwrap(), "l1\nl2\n");
    }
}

/// A [`ContextSink`] backed by a [`SessionLog`].
///
/// The loop records from inside an async generator while the surfaces still
/// own the file, so the handle is shared behind a mutex. Writes are
/// serialized, which is what the log wants anyway: it is an ordered record,
/// and interleaving two turns' facts would make it unreadable.
pub struct SessionLogSink {
    log: tokio::sync::Mutex<SessionLog>,
    /// Cached so callers can name the file without taking the write lock.
    path: PathBuf,
}

impl SessionLogSink {
    pub fn new(log: SessionLog) -> Self {
        Self {
            path: log.path().to_path_buf(),
            log: tokio::sync::Mutex::new(log),
        }
    }

    /// The file being written. Callers index or list it after the session.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait::async_trait]
impl wingman_core::ContextSink for SessionLogSink {
    async fn record(&self, fact: ContextFact) {
        // Best-effort: a session that cannot write its log must still answer
        // the user. The failure is worth knowing about, so it is logged once
        // per occurrence rather than silently dropped.
        if let Err(e) = self.log.lock().await.record_fact(&fact).await {
            tracing::warn!(target: "wingman::session", "could not record session fact: {e}");
        }
    }
}

/// Stable short hash of a system prompt, for `SessionStart.system_hash`.
///
/// Identity, not content: two sessions with the same hash ran under the same
/// base prompt, and a changed hash explains why an otherwise identical replay
/// behaves differently. The prompt itself can be long and holds the user's
/// memories, so the log records a fingerprint rather than a copy.
pub fn system_hash(prompt: &str) -> String {
    // FNV-1a: no dependency, and this is an identity check, not a security
    // boundary.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in prompt.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod projection_tests {
    use super::*;

    fn tool_result(id: &str, output: &str, model_output: Option<&str>) -> SessionRecord {
        SessionRecord::ToolResult {
            ts: "t".into(),
            id: id.into(),
            output: output.into(),
            model_output: model_output.map(str::to_string),
            is_error: false,
        }
    }

    fn contents(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .flat_map(|m| &m.content)
            .map(|b| match b {
                ContentBlock::Text { text } => text.clone(),
                ContentBlock::ToolResult { content, .. } => content.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// The whole point: a resumed conversation must be the one the model had,
    /// not the richer one the tool produced.
    #[test]
    fn a_truncated_result_replays_as_the_model_saw_it() {
        let records = vec![
            SessionRecord::User {
                ts: "t".into(),
                text: "go".into(),
            },
            tool_result("c1", "FULL 4000 LINES", Some("head … elided … tail")),
        ];
        let msgs = records_to_messages(&records);
        let seen = contents(&msgs);
        assert!(seen.iter().any(|c| c == "head … elided … tail"));
        assert!(
            !seen.iter().any(|c| c.contains("FULL 4000 LINES")),
            "replay handed the model more than it originally had"
        );
    }

    #[test]
    fn a_result_the_model_saw_whole_replays_whole() {
        let records = vec![tool_result("c1", "short", None)];
        assert_eq!(contents(&records_to_messages(&records)), vec!["short"]);
    }

    #[test]
    fn compaction_replays_as_a_recap_not_the_folded_messages() {
        let records = vec![
            SessionRecord::User {
                ts: "t".into(),
                text: "first".into(),
            },
            SessionRecord::Assistant {
                ts: "t".into(),
                blocks: vec![ContentBlock::text("answer one")],
            },
            SessionRecord::Recap {
                ts: "t".into(),
                replaced: 2,
                text: "[recap] we discussed one thing".into(),
            },
            SessionRecord::User {
                ts: "t".into(),
                text: "second".into(),
            },
        ];
        let seen = contents(&records_to_messages(&records));
        assert_eq!(
            seen,
            vec!["[recap] we discussed one thing", "second"],
            "the folded messages must not come back — the model no longer had them"
        );
    }

    #[test]
    fn a_pruned_result_replays_pruned() {
        let records = vec![
            tool_result("c1", "the whole thing", Some("the whole thing")),
            SessionRecord::User {
                ts: "t".into(),
                text: "next".into(),
            },
            SessionRecord::ToolResultPruned {
                ts: "t".into(),
                id: "c1".into(),
                content: "head … pruned … tail".into(),
            },
        ];
        let seen = contents(&records_to_messages(&records));
        assert!(seen.iter().any(|c| c == "head … pruned … tail"));
        assert!(!seen.iter().any(|c| c == "the whole thing"));
    }

    #[test]
    fn injected_context_is_recorded_but_is_not_message_history() {
        // It rode the system prompt, which is rebuilt per turn rather than
        // replayed — but a reader asking "why did it do that" can still see it.
        let records = vec![
            SessionRecord::InjectedContext {
                ts: "t".into(),
                text: "remembered: the build is slow".into(),
            },
            SessionRecord::User {
                ts: "t".into(),
                text: "go".into(),
            },
        ];
        assert_eq!(contents(&records_to_messages(&records)), vec!["go"]);
    }

    /// Old logs predate every field and variant added here.
    #[test]
    fn a_log_written_before_this_change_still_loads() {
        let line = r#"{"kind":"tool_result","ts":"t","id":"c1","output":"hi","is_error":false}"#;
        let rec: SessionRecord = serde_json::from_str(line).expect("old records must still parse");
        match rec {
            SessionRecord::ToolResult { model_output, .. } => assert!(model_output.is_none()),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn the_system_hash_is_stable_and_distinguishes_prompts() {
        assert_eq!(
            system_hash("you are wingman"),
            system_hash("you are wingman")
        );
        assert_ne!(system_hash("you are wingman"), system_hash("you are other"));
    }
}
