//! E10 — manager↔worker bidirectional IPC (protocol layer).
//!
//! M1 dispatches workers one-shot; the `message_agent` tool only logs.
//! E10 gives the manager a stdin command channel into a live worker and a
//! way for the worker to ask questions without dying. This module defines
//! the wire protocol — newline-delimited JSON in both directions — and the
//! encode/parse functions. The actual pipe plumbing lives in the worker
//! supervisor (`child_process.rs`); keeping the protocol pure makes it
//! exhaustively testable.
//!
//! Manager → worker: [`ManagerCommand`] (pivot / cancel / clarify).
//! Worker → manager: [`WorkerMessage`] (question / ack / done).

use serde::{Deserialize, Serialize};

/// A command the manager sends down a worker's stdin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ManagerCommand {
    /// Append new context and a revised goal mid-task — the worker keeps
    /// its progress but re-orients.
    Pivot { goal: String, context: String },
    /// Abort cleanly; commit partial work to a side branch first.
    Cancel { reason: String },
    /// Answer a question the worker raised, unblocking it.
    Clarify { answer: String },
}

/// A message a worker pushes up to the manager (in addition to the normal
/// task event stream).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum WorkerMessage {
    /// The worker needs an answer to proceed; the manager replies with a
    /// [`ManagerCommand::Clarify`].
    Question { text: String },
    /// The worker received and applied a command.
    Ack { command: String },
    /// The worker is pausing, waiting for a clarify.
    Blocked { on: String },
}

/// Encode a command as a single NDJSON line (no trailing newline; the
/// writer adds it).
pub fn encode_command(cmd: &ManagerCommand) -> String {
    serde_json::to_string(cmd).expect("ManagerCommand is always serialisable")
}

/// Parse a command line sent by the manager.
pub fn parse_command(line: &str) -> Result<ManagerCommand, String> {
    serde_json::from_str(line.trim()).map_err(|e| format!("bad manager command: {e}"))
}

/// Encode a worker message as a single NDJSON line.
pub fn encode_message(msg: &WorkerMessage) -> String {
    serde_json::to_string(msg).expect("WorkerMessage is always serialisable")
}

/// Parse a worker message line. Returns `Ok(None)` for lines that aren't
/// IPC messages (e.g. ordinary task-event JSON sharing the same stdout),
/// so the supervisor can tee the stream without erroring on every event.
pub fn parse_message(line: &str) -> Result<Option<WorkerMessage>, String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let v: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| format!("bad worker line: {e}"))?;
    // Only lines tagged with "msg" are IPC messages.
    if v.get("msg").is_none() {
        return Ok(None);
    }
    serde_json::from_value(v)
        .map(Some)
        .map_err(|e| format!("bad worker message: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pivot_roundtrips() {
        let cmd = ManagerCommand::Pivot {
            goal: "use the new schema".into(),
            context: "the migration landed in main".into(),
        };
        let line = encode_command(&cmd);
        assert!(line.contains(r#""cmd":"pivot""#));
        assert_eq!(parse_command(&line).unwrap(), cmd);
    }

    #[test]
    fn cancel_roundtrips() {
        let cmd = ManagerCommand::Cancel {
            reason: "superseded".into(),
        };
        assert_eq!(parse_command(&encode_command(&cmd)).unwrap(), cmd);
    }

    #[test]
    fn clarify_roundtrips() {
        let cmd = ManagerCommand::Clarify {
            answer: "use anyhow".into(),
        };
        assert_eq!(parse_command(&encode_command(&cmd)).unwrap(), cmd);
    }

    #[test]
    fn parse_command_rejects_garbage() {
        assert!(parse_command("{}").is_err());
        assert!(parse_command("not json").is_err());
    }

    #[test]
    fn question_message_roundtrips() {
        let msg = WorkerMessage::Question {
            text: "which config file?".into(),
        };
        let line = encode_message(&msg);
        assert!(line.contains(r#""msg":"question""#));
        assert_eq!(parse_message(&line).unwrap(), Some(msg));
    }

    #[test]
    fn non_ipc_line_parses_to_none() {
        // An ordinary task event sharing stdout — not an IPC message.
        let line = r#"{"event":"task_progress","tool":"edit_file"}"#;
        assert_eq!(parse_message(line).unwrap(), None);
    }

    #[test]
    fn blank_line_is_none() {
        assert_eq!(parse_message("   ").unwrap(), None);
    }

    #[test]
    fn malformed_json_errors() {
        assert!(parse_message("{not json").is_err());
    }

    #[test]
    fn ack_and_blocked_roundtrip() {
        for msg in [
            WorkerMessage::Ack {
                command: "pivot".into(),
            },
            WorkerMessage::Blocked {
                on: "clarify".into(),
            },
        ] {
            assert_eq!(parse_message(&encode_message(&msg)).unwrap(), Some(msg));
        }
    }
}
