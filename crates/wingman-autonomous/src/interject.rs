//! J4 — conversational mid-run interjection.
//!
//! Once a run is in flight the user can redirect it without killing it:
//!
//! ```text
//! wingman pilot tell <run-id> "skip the changelog task"
//! wingman pilot ask  <run-id> "what files have you touched so far?"
//! ```
//!
//! Or a reply in the same Slack/GitHub thread routes to the active run.
//! This module parses the interjection and maps it onto the E10 IPC
//! ([`crate::ipc`]) the manager already understands: `tell` becomes a
//! pivot/context injection, `ask` becomes a query the manager answers
//! between tool calls. Delivering it down the live channel is the
//! orchestrator's job.

use crate::ipc::ManagerCommand;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterjectKind {
    /// Inject guidance the manager should act on (no reply expected).
    Tell,
    /// Ask a question and block on the manager's answer.
    Ask,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interjection {
    pub run_id: String,
    pub kind: InterjectKind,
    pub message: String,
}

/// What the orchestrator dispatches for an interjection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    /// Send this command down the worker/manager IPC channel.
    Command(ManagerCommand),
    /// Pose a question to the manager and await a reply.
    Query(String),
}

impl Interjection {
    pub fn to_dispatch(&self) -> Dispatch {
        match self.kind {
            InterjectKind::Tell => Dispatch::Command(ManagerCommand::Pivot {
                goal: self.message.clone(),
                context: "user interjection via `pilot tell`".to_string(),
            }),
            InterjectKind::Ask => Dispatch::Query(self.message.clone()),
        }
    }
}

/// Parse a CLI-style interjection: `tell <run> <message…>` /
/// `ask <run> <message…>`. The verb and run-id are the first two tokens;
/// the rest is the message.
pub fn parse(verb: &str, run_id: &str, message: &str) -> Result<Interjection, String> {
    let kind = match verb.trim().to_ascii_lowercase().as_str() {
        "tell" => InterjectKind::Tell,
        "ask" => InterjectKind::Ask,
        other => {
            return Err(format!(
                "unknown interjection verb `{other}` (expect tell|ask)"
            ))
        }
    };
    if run_id.trim().is_empty() {
        return Err("interjection needs a run id".to_string());
    }
    if message.trim().is_empty() {
        return Err("interjection message is empty".to_string());
    }
    Ok(Interjection {
        run_id: run_id.trim().to_string(),
        kind,
        message: message.trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tell() {
        let i = parse("tell", "r1", "skip the changelog task").unwrap();
        assert_eq!(i.kind, InterjectKind::Tell);
        assert_eq!(i.run_id, "r1");
        assert_eq!(i.message, "skip the changelog task");
    }

    #[test]
    fn parse_ask_case_insensitive() {
        let i = parse("ASK", "r1", "what files?").unwrap();
        assert_eq!(i.kind, InterjectKind::Ask);
    }

    #[test]
    fn parse_rejects_unknown_verb() {
        assert!(parse("yell", "r1", "x").is_err());
    }

    #[test]
    fn parse_rejects_empty_run_and_message() {
        assert!(parse("tell", "", "x").is_err());
        assert!(parse("tell", "r1", "  ").is_err());
    }

    #[test]
    fn tell_maps_to_pivot_command() {
        let i = parse("tell", "r1", "use the new schema").unwrap();
        match i.to_dispatch() {
            Dispatch::Command(ManagerCommand::Pivot { goal, .. }) => {
                assert_eq!(goal, "use the new schema");
            }
            other => panic!("expected pivot, got {other:?}"),
        }
    }

    #[test]
    fn ask_maps_to_query() {
        let i = parse("ask", "r1", "status?").unwrap();
        assert_eq!(i.to_dispatch(), Dispatch::Query("status?".to_string()));
    }
}
