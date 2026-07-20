//! `message_agent` — send a mid-run message to a running worker.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_tools::{Tool, ToolCtx};

use crate::orchestrator::OrchestratorHandle;

pub struct MessageAgent {
    handle: OrchestratorHandle,
}

impl MessageAgent {
    pub fn new(handle: OrchestratorHandle) -> Self {
        Self { handle }
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    agent_id: String,
    body: String,
}

#[async_trait]
impl Tool for MessageAgent {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "message_agent".into(),
            description: "Send a mid-run message to a worker over its stdin command channel \
                 (E10). Supported message kinds: `pivot` (revise goal), `cancel` \
                 (clean abort), `clarify` (answer a worker's question). Phase 4 logs \
                 the message; Phase 7.5 wires the actual stdin channel."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string"},
                    "body": {"type": "string"}
                },
                "required": ["agent_id", "body"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        match self.handle.message_agent(&args.agent_id, &args.body).await {
            Ok(()) => ToolOutcome::ok(format!("message queued for {}", args.agent_id)),
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    }
}
