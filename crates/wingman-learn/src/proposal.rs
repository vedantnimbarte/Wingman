//! Detection heuristics for when the agent should propose persisting a
//! memory or skill.
//!
//! The actual draft-generation is left to the model itself: the system
//! prompt teaches it the `save_memory` / `save_skill` tools, and the
//! [`needs_nudge`] / [`looks_persistable`] helpers below decide when to
//! lean on it.

use wingman_core::{ContentBlock, Message, Role};

/// Words/phrases the user might say to explicitly ask for persistence.
const PERSIST_TRIGGERS: &[&str] = &[
    "remember",
    "save this",
    "save that",
    "next time",
    "from now on",
    "always",
    "make a skill",
    "as a skill",
    "as a memory",
    "note for later",
    "keep in mind",
    "i prefer",
];

/// Heuristic check on the latest user message — does it look like an
/// explicit request to persist something?
pub fn user_asked_to_persist(history: &[Message]) -> bool {
    let Some(last) = history.iter().rev().find(|m| m.role == Role::User) else {
        return false;
    };
    let text = user_text(last).to_ascii_lowercase();
    if text.is_empty() {
        return false;
    }
    PERSIST_TRIGGERS.iter().any(|t| text.contains(t))
}

/// Cheap "is this conversation deep enough that something is worth keeping?"
/// heuristic. We require at least N assistant turns *and* at least one
/// tool_use block (so we know the agent actually did work, not just chatted).
pub fn looks_persistable(history: &[Message], min_assistant_turns: usize) -> bool {
    let assistant_turns = history.iter().filter(|m| m.role == Role::Assistant).count();
    if assistant_turns < min_assistant_turns {
        return false;
    }
    history.iter().any(|m| {
        m.role == Role::Assistant
            && m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
    })
}

/// "How many sessions in a row has the user gone without saving anything?"
/// Past this threshold, the hook injects a one-line nudge into the system
/// prompt encouraging the agent to propose a memory or skill.
pub const NUDGE_AFTER_N_QUIET_SESSIONS: i64 = 5;

/// Render the one-line nudge string. Lives here so tests can lock the wording.
pub fn nudge_line() -> &'static str {
    "Note: it has been several sessions since anything was saved to memory. \
     If something useful or surprising comes up this turn, consider calling \
     the `save_memory` tool (or proposing a new skill) so it isn't lost."
}

fn user_text(m: &Message) -> String {
    let mut out = String::new();
    for b in &m.content {
        if let ContentBlock::Text { text } = b {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wingman_core::Message;

    #[test]
    fn user_asked_to_persist_picks_up_remember() {
        let h = vec![Message::user_text("Hey, remember that I use pnpm not npm")];
        assert!(user_asked_to_persist(&h));
    }

    #[test]
    fn user_asked_to_persist_negative_when_neutral() {
        let h = vec![Message::user_text("What does the function do?")];
        assert!(!user_asked_to_persist(&h));
    }

    #[test]
    fn looks_persistable_requires_work() {
        let chatty: Vec<Message> = (0..10)
            .map(|_| Message::assistant(vec![ContentBlock::text("hello")]))
            .collect();
        assert!(!looks_persistable(&chatty, 3));
    }
}
