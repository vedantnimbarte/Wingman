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
use wingman_core::{AgentEvent, ContentBlock, Message, Role, Usage};

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
                is_error: false,
            },
            SessionRecord::ToolResult {
                ts: "t".into(),
                id: "b".into(),
                output: "rb".into(),
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
