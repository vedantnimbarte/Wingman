//! Append-only JSONL session log.
//!
//! One file per session under `<project>/.arccode/sessions/<timestamp>.jsonl`.
//! Each record is a single line of JSON. Records are typed via a `kind` field
//! so a reader can interleave user prompts, assistant text/tool calls,
//! results, and usage updates.
//!
//! Future M4 work (`/resume`) reads the same file back; the format must
//! remain backwards-compatible — only additive fields.

use std::path::{Path, PathBuf};

use arccode_core::{AgentEvent, ContentBlock, Message, Role, Usage};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
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
        output: String,
        is_error: bool,
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

    pub fn path(&self) -> &Path {
        &self.path
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
                                is_error: *is_error,
                            })
                            .await?;
                        }
                        ContentBlock::ToolUse { .. } => { /* should not appear from user */ }
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
                    reason: serde_json::to_string(reason).unwrap_or_else(|_| "\"unknown\"".into()),
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
                is_error,
                ..
            } => {
                pending_tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: output.clone(),
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
            _ => {
                // SessionStart, UsageDelta, Stop — ignored for history reconstruction.
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
        if let SessionRecord::SessionStart { provider, model, .. } = record {
            return Some((provider.clone(), model.clone()));
        }
    }
    None
}
