//! Full-stack integration: the real agent loop driving the real tool registry.
//!
//! The `wingman-core` unit tests exercise the loop with a *mock* tool
//! dispatcher; this exercises the loop against the genuine
//! `wingman_tools::ToolRegistry` (with built-ins, permission gating, and real
//! file I/O). A scripted provider stands in for the LLM so no network/keys are
//! needed. It proves the layers wire together end-to-end: provider →
//! tool_use → ToolRegistry::dispatch → real `read_file` → tool_result → stop.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use wingman_config::PermissionMode;
use wingman_core::{
    AgentConfig, AgentEvent, AgentLoop, CompletionRequest, ContentBlock, Provider,
    ProviderCapabilities, ProviderEventStream, StopReason, StreamEvent,
};
use wingman_tools::{ToolCtx, ToolRegistry};

/// Replays one scripted response per `complete` call.
struct ScriptedProvider {
    responses: Mutex<VecDeque<Vec<StreamEvent>>>,
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "scripted"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tools: true,
            vision: false,
            cache_kind: wingman_core::CacheKind::None,
        }
    }
    async fn complete(&self, _req: CompletionRequest) -> wingman_core::Result<ProviderEventStream> {
        let events = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("provider called more times than scripted");
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }
}

#[tokio::test]
async fn agent_loop_executes_a_real_read_file_tool() {
    // A project dir with a file to read.
    let dir = std::env::temp_dir().join(format!("wm-agentloop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("hello.txt");
    std::fs::write(&file, "INTEGRATION_MARKER_42").unwrap();

    // The real registry with built-ins, read-only, rooted at the temp project.
    let ctx = ToolCtx::new(PermissionMode::ReadOnly, dir.clone(), dir.clone());
    let registry: Arc<dyn wingman_core::ToolDispatcher> =
        Arc::new(ToolRegistry::new(ctx).with_builtins());

    // Turn 1: the model asks to read the file. Turn 2: it replies and ends.
    let provider = Arc::new(ScriptedProvider {
        responses: Mutex::new(
            vec![
                vec![
                    StreamEvent::ToolUse {
                        block: ContentBlock::ToolUse {
                            id: "call-1".into(),
                            name: "read_file".into(),
                            input: serde_json::json!({ "path": file.to_string_lossy() }),
                        },
                    },
                    StreamEvent::Stop {
                        reason: StopReason::ToolUse,
                    },
                ],
                vec![
                    StreamEvent::TextDelta {
                        text: "done".into(),
                    },
                    StreamEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ],
            ]
            .into(),
        ),
    });

    let config = AgentConfig {
        model: "scripted/test".into(),
        ..AgentConfig::default()
    };
    let mut agent = AgentLoop::new(provider, registry, config);

    let mut events = Vec::new();
    let mut stream = agent.run("read hello.txt".into());
    while let Some(ev) = stream.next().await {
        events.push(ev);
    }

    // The real read_file must have executed and returned the file's content.
    let tool_result = events.iter().find_map(|e| match e {
        AgentEvent::ToolResult {
            output, is_error, ..
        } => Some((output.clone(), *is_error)),
        _ => None,
    });
    let (output, is_error) = tool_result.expect("expected a ToolResult from read_file");
    assert!(!is_error, "read_file should not error: {output}");
    assert!(
        output.contains("INTEGRATION_MARKER_42"),
        "read_file output should contain the file content, got: {output}"
    );

    // And the loop must have reached a clean stop.
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Stop { .. })));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn read_outside_project_is_blocked_by_permission_gate() {
    // The registry must enforce ToolCtx read confinement end-to-end: a path
    // outside the project root is refused even though the tool exists.
    let dir = std::env::temp_dir().join(format!("wm-agentloop-deny-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = ToolCtx::new(PermissionMode::ReadOnly, dir.clone(), dir.clone());
    let registry: Arc<dyn wingman_core::ToolDispatcher> =
        Arc::new(ToolRegistry::new(ctx).with_builtins());

    // Point read_file at an out-of-tree path.
    let outside = std::env::temp_dir().join("definitely-outside-the-project.txt");
    let provider = Arc::new(ScriptedProvider {
        responses: Mutex::new(
            vec![
                vec![
                    StreamEvent::ToolUse {
                        block: ContentBlock::ToolUse {
                            id: "c1".into(),
                            name: "read_file".into(),
                            input: serde_json::json!({ "path": outside.to_string_lossy() }),
                        },
                    },
                    StreamEvent::Stop {
                        reason: StopReason::ToolUse,
                    },
                ],
                vec![
                    StreamEvent::TextDelta { text: "ok".into() },
                    StreamEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ],
            ]
            .into(),
        ),
    });

    let config = AgentConfig {
        model: "scripted/test".into(),
        ..AgentConfig::default()
    };
    let mut agent = AgentLoop::new(provider, registry, config);
    let mut stream = agent.run("read a secret".into());
    let mut denied = false;
    while let Some(ev) = stream.next().await {
        if let AgentEvent::ToolResult { is_error, .. } = ev {
            if is_error {
                denied = true;
            }
        }
    }
    assert!(denied, "reading outside the project root should be refused");
    let _ = std::fs::remove_dir_all(&dir);
}
