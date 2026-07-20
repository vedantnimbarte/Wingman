//! `reassign_task` — kill the current worker and spawn a fresh one.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_tools::{Tool, ToolCtx};

use crate::orchestrator::OrchestratorHandle;

pub struct ReassignTask {
    handle: OrchestratorHandle,
}

impl ReassignTask {
    pub fn new(handle: OrchestratorHandle) -> Self {
        Self { handle }
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    task_id: String,
}

#[async_trait]
impl Tool for ReassignTask {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "reassign_task".into(),
            description: "Abort the current worker (if any), reset the task to `todo`, and \
                 spawn a fresh worker. Rung-2 retry: same task, new worker, escalated \
                 model context if the manager passes one in the next task assignment."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string"}
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        match self.handle.reassign(&args.task_id).await {
            Ok(agent_id) => {
                ToolOutcome::ok(format!("reassigned task {} to {agent_id}", args.task_id))
            }
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    }
}
