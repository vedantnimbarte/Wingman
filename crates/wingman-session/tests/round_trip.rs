//! The invariant, end to end: run a real turn through the agent loop, let it
//! record to a real session log, then reconstruct the conversation from that
//! log and check it is the one the model actually had.
//!
//! This is the regression test for the bug that motivated the change — the
//! TUI wrote its own log from the prompt text and the streamed assistant
//! text, so a session's log contained no tool calls at all and `/resume`
//! rebuilt a conversation in which the agent had never used a tool.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use wingman_core::{
    agent::{AgentConfig, AgentEvent, AgentLoop, ToolDispatcher, ToolOutcome},
    provider::{Provider, ProviderCapabilities},
    stream::{ProviderEventStream, StopReason, StreamEvent},
    CompletionRequest, ContentBlock, Message, ToolOutputBudget, ToolSpec,
};
use wingman_session::{records_to_messages, SessionLog, SessionLogSink};

struct Replay(Mutex<VecDeque<Vec<StreamEvent>>>);

#[async_trait]
impl Provider for Replay {
    fn id(&self) -> &str {
        "replay"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tools: true,
            vision: false,
            cache_kind: wingman_core::CacheKind::None,
            reasoning: false,
        }
    }
    async fn complete(&self, _req: CompletionRequest) -> wingman_core::Result<ProviderEventStream> {
        let events = self.0.lock().unwrap().pop_front().expect("over-called");
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }
}

struct Chatty(usize);

#[async_trait]
impl ToolDispatcher for Chatty {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    async fn dispatch(&self, _name: &str, _args: serde_json::Value) -> ToolOutcome {
        ToolOutcome::ok(
            (0..self.0)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

/// One turn: assistant says something and calls a tool, then ends.
fn script() -> VecDeque<Vec<StreamEvent>> {
    VecDeque::from(vec![
        vec![
            StreamEvent::TextDelta {
                text: "let me look".into(),
            },
            StreamEvent::ToolUse {
                block: ContentBlock::ToolUse {
                    id: "t0".into(),
                    name: "grep_tool".into(),
                    input: serde_json::json!({"pattern": "x"}),
                },
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
            },
        ],
        vec![
            StreamEvent::TextDelta {
                text: "found it".into(),
            },
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ],
    ])
}

/// Returns what was sent, what the log reconstructs, and the tempdir — which
/// the caller must hold, or it is removed while the assertions run.
async fn run_and_reconstruct(tool_lines: usize) -> (Vec<Message>, Vec<Message>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let log = SessionLog::create(dir.path()).await.unwrap();
    let path = log.path().to_path_buf();
    let sink = Arc::new(SessionLogSink::new(log));

    let mut agent = AgentLoop::new(
        Arc::new(Replay(Mutex::new(script()))),
        Arc::new(Chatty(tool_lines)),
        AgentConfig {
            model: "m".into(),
            tool_output_budget: ToolOutputBudget::new(10),
            context_sink: Some(sink.clone()),
            ..Default::default()
        },
    );

    let mut stream = agent.run("find x".into());
    while let Some(ev) = futures::StreamExt::next(&mut stream).await {
        if matches!(ev, AgentEvent::Stop { .. }) {
            break;
        }
    }
    drop(stream);

    let sent = agent.history().to_vec();
    // Give the sink's buffered writes a moment to land on disk.
    let records = wingman_session::load_session(&path).unwrap();
    let rebuilt = records_to_messages(&records);
    (sent, rebuilt, dir)
}

#[tokio::test]
async fn the_log_reconstructs_the_conversation_the_model_had() {
    let (sent, rebuilt, _dir) = run_and_reconstruct(3).await;

    assert_eq!(
        sent.len(),
        rebuilt.len(),
        "log yields a different number of messages than were sent:\nsent: {sent:#?}\nlog: {rebuilt:#?}"
    );
    for (i, (a, b)) in sent.iter().zip(&rebuilt).enumerate() {
        assert_eq!(a.role, b.role, "message {i} changed role");
    }

    // The specific regression: the tool call must survive into the log.
    let has_tool_use = rebuilt
        .iter()
        .flat_map(|m| &m.content)
        .any(|b| matches!(b, ContentBlock::ToolUse { name, .. } if name == "grep_tool"));
    assert!(
        has_tool_use,
        "the reconstructed conversation has no tool call — this is exactly the bug"
    );
    let has_result = rebuilt
        .iter()
        .flat_map(|m| &m.content)
        .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
    assert!(has_result, "the tool result did not survive into the log");
}

#[tokio::test]
async fn a_truncated_result_reconstructs_as_the_bounded_form() {
    // 500 lines against a 10-line budget: the model saw a truncated view, and
    // the log must hand that same view back rather than the full text.
    let (sent, rebuilt, _dir) = run_and_reconstruct(500).await;
    let sent_result = sent
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("a tool result was sent");
    let rebuilt_result = rebuilt
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("a tool result was logged");
    assert_eq!(
        sent_result, rebuilt_result,
        "replay would hand the model a different result than it originally saw"
    );
    assert!(rebuilt_result.contains("elided"));
}
