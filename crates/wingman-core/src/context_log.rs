//! What the model saw, recorded as it happens.
//!
//! The agent loop keeps conversation history in memory and each surface used
//! to write the session log by hand, from whatever it happened to have kept.
//! They disagreed: the TUI recorded the user prompt and the assistant's text
//! and nothing else — no tool calls, no tool results — so resuming a TUI
//! session rebuilt a conversation in which the agent never used a tool.
//! Headless recorded more. `serve` recorded differently again.
//!
//! The loop is the only place that knows what actually went into a request,
//! so it is the only place that can record it faithfully. It emits a
//! [`ContextFact`] at every point it changes what the model will see; a
//! [`ContextSink`] writes them down. Surfaces provide the sink and stop
//! guessing.
//!
//! The invariant this exists to hold: **model-visible means logged.** Adding
//! a new kind of model-visible input means adding a fact here, which is the
//! point — it is hard to slip something into the model's context without
//! also writing it down.
//!
//! Kept separate from [`AgentEvent`](crate::agent::AgentEvent), which is the
//! UI stream and a public interface (`--print --json`). The two answer
//! different questions: `AgentEvent` is "what should I show a human right
//! now", `ContextFact` is "what did the model actually receive".

use crate::{ContentBlock, Usage};
use async_trait::async_trait;

/// One durable change to what the model sees.
#[derive(Debug, Clone)]
pub enum ContextFact {
    /// A user-role message entered the conversation. Covers the human's
    /// prompt and loop-authored messages such as verification-gate feedback,
    /// which are equally model-visible and were previously unrecorded.
    UserMessage { text: String },
    /// An assistant message, including any `tool_use` blocks. The blocks
    /// matter: without them a resumed conversation has tool results whose
    /// calls are missing, which no provider accepts.
    AssistantMessage { blocks: Vec<ContentBlock> },
    /// One tool result.
    ///
    /// `full` is what the tool produced and what a human is shown; `model_view`
    /// is the bounded form actually sent, when they differ (truncated, and
    /// carrying a spill locator). Both are kept: replacing `full` would gut
    /// the audit trail, and recording only `full` is how the log came to claim
    /// the model had seen more than it did.
    ToolResult {
        id: String,
        full: String,
        model_view: Option<String>,
        is_error: bool,
    },
    /// Compaction folded the oldest `replaced` messages into `recap`.
    Compacted { replaced: usize, recap: String },
    /// An earlier tool result was shrunk in place to reclaim context.
    ToolResultPruned { id: String, content: String },
    /// Text spliced onto the system prompt for one turn (memory recall,
    /// nudges, an injected skill body). Not part of the message history, so
    /// it does not replay — but it changed the request, so a reader asking
    /// "why did it do that" needs to see it.
    SystemInjected { text: String },
    /// Cumulative token usage for the turn.
    Usage { usage: Usage },
    /// The turn ended.
    Stop { reason: String },
}

/// Somewhere to put [`ContextFact`]s. Implemented over the session log.
///
/// Recording is best-effort and must never fail a turn: a session that cannot
/// write its log should still answer the user. Implementations swallow their
/// own errors rather than propagating them.
#[async_trait]
pub trait ContextSink: Send + Sync {
    async fn record(&self, fact: ContextFact);
}

/// Assert that a request's history matches what was recorded.
///
/// The invariant this module exists to hold is that the session log can
/// reconstruct what the model saw. That is easy to state and easy to break:
/// any future code that mutates history without emitting a [`ContextFact`]
/// silently reintroduces exactly the drift this replaced. This check is what
/// notices.
///
/// Debug builds only — it walks the whole history, and in release the cost is
/// not worth paying on every turn.
#[cfg(debug_assertions)]
pub fn debug_assert_reconstructs(sent: &[crate::Message], reconstructed: &[crate::Message]) {
    if sent.len() != reconstructed.len() {
        debug_assert!(
            false,
            "session log does not reconstruct the conversation: sent {} messages, log yields {}. \
             Something mutated history without recording a ContextFact.",
            sent.len(),
            reconstructed.len()
        );
        return;
    }
    for (i, (a, b)) in sent.iter().zip(reconstructed).enumerate() {
        debug_assert_eq!(
            a.role, b.role,
            "message {i} changed role between the request and the log"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentBlock, Message};
    use std::sync::Mutex;

    /// Collects facts so a test can assert on what the loop recorded.
    #[derive(Default)]
    pub struct Recorder(pub Mutex<Vec<ContextFact>>);

    #[async_trait]
    impl ContextSink for Recorder {
        async fn record(&self, fact: ContextFact) {
            self.0.lock().unwrap().push(fact);
        }
    }

    #[tokio::test]
    async fn a_sink_receives_facts_in_order() {
        let r = Recorder::default();
        r.record(ContextFact::UserMessage { text: "hi".into() })
            .await;
        r.record(ContextFact::AssistantMessage {
            blocks: vec![ContentBlock::text("hello")],
        })
        .await;
        let got = r.0.lock().unwrap();
        assert_eq!(got.len(), 2);
        assert!(matches!(got[0], ContextFact::UserMessage { .. }));
    }

    #[test]
    fn the_invariant_check_accepts_a_faithful_reconstruction() {
        let sent = vec![Message::user_text("a"), Message::assistant(vec![])];
        let same = sent.clone();
        debug_assert_reconstructs(&sent, &same);
    }
}
