//! Cross-process control channel for a live pilot run.
//!
//! The orchestrator actor runs inside the `pilot run` / `pilot resume`
//! process. Other processes — `pilot watch`, `pilot abort`, `pilot approve` —
//! can't call it directly, so they append newline-delimited JSON commands to
//! `<run-dir>/control.jsonl`. The orchestrator's control watchdog
//! ([`crate::orchestrator`]) tails that file and applies each command exactly
//! once.
//!
//! The wire format is deliberately tiny and human-writable so an operator can
//! `echo '{"cmd":"abort_run"}' >> control.jsonl` in a pinch. Parsing is
//! lenient: blank lines and unrecognised commands are skipped rather than
//! aborting the tail, so a newer writer never wedges an older reader.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Control-file name inside a run directory.
pub const CONTROL_FILE: &str = "control.jsonl";

/// Path to a run's control file.
pub fn control_path(run_dir: &Path) -> PathBuf {
    run_dir.join(CONTROL_FILE)
}

/// A single operator command addressed to a live run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ControlCommand {
    /// Abort the whole run: cancel in-flight workers and stop.
    AbortRun,
    /// Abort one task's in-flight worker.
    AbortTask { id: String },
    /// Re-queue a failed / blocked task for another attempt.
    RetryTask { id: String },
    /// Release the plan-approval gate so execution begins.
    Approve,
    /// Reject the pending plan; the run aborts before execution.
    Veto,
}

impl ControlCommand {
    /// Serialise to a single JSON line (no trailing newline).
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("control command serializes")
    }

    /// Parse one line, returning `None` for blank lines or anything that
    /// isn't a recognised command.
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        serde_json::from_str(line).ok()
    }
}

/// Append a command to the run's control file, creating it if needed.
pub fn append(run_dir: &Path, cmd: &ControlCommand) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(control_path(run_dir))?;
    writeln!(f, "{}", cmd.encode())
}

/// Tails the control file, remembering how far it has consumed so each
/// command is applied exactly once across polls.
#[derive(Debug, Default)]
pub struct ControlReader {
    /// Byte offset consumed so far.
    offset: u64,
}

impl ControlReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read and parse any commands appended since the last poll. A missing
    /// file yields nothing; a file that shrank (deleted / rotated) is re-read
    /// from the top so we never silently skip commands.
    pub fn poll(&mut self, run_dir: &Path) -> Vec<ControlCommand> {
        let Ok(bytes) = std::fs::read(control_path(run_dir)) else {
            return Vec::new();
        };
        let len = bytes.len() as u64;
        if len < self.offset {
            self.offset = 0;
        }
        if len == self.offset {
            return Vec::new();
        }
        let start = self.offset as usize;
        self.offset = len;
        String::from_utf8_lossy(&bytes[start..])
            .lines()
            .filter_map(ControlCommand::parse)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn commands_round_trip_through_json() {
        let cases = [
            ControlCommand::AbortRun,
            ControlCommand::AbortTask { id: "t3".into() },
            ControlCommand::RetryTask { id: "t5".into() },
            ControlCommand::Approve,
            ControlCommand::Veto,
        ];
        for c in cases {
            assert_eq!(ControlCommand::parse(&c.encode()), Some(c));
        }
    }

    #[test]
    fn encoding_is_the_documented_shape() {
        assert_eq!(ControlCommand::AbortRun.encode(), r#"{"cmd":"abort_run"}"#);
        assert_eq!(
            ControlCommand::AbortTask { id: "t3".into() }.encode(),
            r#"{"cmd":"abort_task","id":"t3"}"#
        );
    }

    #[test]
    fn parse_skips_blank_and_garbage_lines() {
        assert_eq!(ControlCommand::parse("   "), None);
        assert_eq!(ControlCommand::parse("not json"), None);
        assert_eq!(ControlCommand::parse(r#"{"cmd":"nope"}"#), None);
        // A hand-written command still parses.
        assert_eq!(
            ControlCommand::parse(r#"  {"cmd":"approve"}  "#),
            Some(ControlCommand::Approve)
        );
    }

    #[test]
    fn reader_returns_only_newly_appended_commands() {
        let dir = tempdir().unwrap();
        let mut reader = ControlReader::new();
        // Nothing yet.
        assert!(reader.poll(dir.path()).is_empty());

        append(dir.path(), &ControlCommand::AbortTask { id: "t1".into() }).unwrap();
        assert_eq!(
            reader.poll(dir.path()),
            vec![ControlCommand::AbortTask { id: "t1".into() }]
        );
        // Second poll with no new writes → empty.
        assert!(reader.poll(dir.path()).is_empty());

        append(dir.path(), &ControlCommand::AbortRun).unwrap();
        assert_eq!(reader.poll(dir.path()), vec![ControlCommand::AbortRun]);
    }

    #[test]
    fn reader_rereads_after_truncation() {
        let dir = tempdir().unwrap();
        let mut reader = ControlReader::new();
        append(dir.path(), &ControlCommand::AbortRun).unwrap();
        assert_eq!(reader.poll(dir.path()), vec![ControlCommand::AbortRun]);

        // Truncate (rotate) the file, then write a fresh command.
        std::fs::write(control_path(dir.path()), b"").unwrap();
        append(dir.path(), &ControlCommand::Veto).unwrap();
        assert_eq!(reader.poll(dir.path()), vec![ControlCommand::Veto]);
    }
}
