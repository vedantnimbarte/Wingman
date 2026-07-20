//! `abort_task` — terminate a worker and mark its task `failed`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_tools::{Tool, ToolCtx};

use crate::orchestrator::OrchestratorHandle;

pub struct AbortTask {
    handle: OrchestratorHandle,
}

impl AbortTask {
    pub fn new(handle: OrchestratorHandle) -> Self {
        Self { handle }
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    task_id: String,
}

#[async_trait]
impl Tool for AbortTask {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "abort_task".into(),
            description: "Terminate the worker assigned to this task (tree-kill) and mark the \
                 task `failed`. Use when a worker is stuck or has drifted past the \
                 goal beyond what reassign can recover from."
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
        match self.handle.abort_task(&args.task_id).await {
            Ok(()) => ToolOutcome::ok(format!("aborted task {}", args.task_id)),
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    }
}
