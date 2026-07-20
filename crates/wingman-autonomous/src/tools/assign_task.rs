//! `assign_task` — pick up a `todo` task and spawn a worker.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_tools::{Tool, ToolCtx};

use crate::orchestrator::OrchestratorHandle;

pub struct AssignTask {
    handle: OrchestratorHandle,
}

impl AssignTask {
    pub fn new(handle: OrchestratorHandle) -> Self {
        Self { handle }
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    task_id: String,
}

#[async_trait]
impl Tool for AssignTask {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "assign_task".into(),
            description: "Spawn a worker for the named task. The orchestrator creates a worktree, \
                 writes task.assign + agent.spawn events, and drives the worker to \
                 completion in the background. Use this when the task is `todo` and all \
                 deps are `done`. Returns the new agent id."
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
        match self.handle.assign_task(&args.task_id).await {
            Ok(agent_id) => {
                ToolOutcome::ok(format!("assigned task {} to {agent_id}", args.task_id))
            }
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    }
}
