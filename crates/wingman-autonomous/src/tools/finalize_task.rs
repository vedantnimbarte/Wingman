//! `finalize_task` — move a `review` task to `done` after merge.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_tools::{Tool, ToolCtx};

use crate::orchestrator::OrchestratorHandle;

pub struct FinalizeTask {
    handle: OrchestratorHandle,
}

impl FinalizeTask {
    pub fn new(handle: OrchestratorHandle) -> Self {
        Self { handle }
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    task_id: String,
    /// Squash-merge commit on the integration branch, if available. The
    /// orchestrator records it as a `run.merge.task` event so the dashboard
    /// can show "merged at <sha>".
    #[serde(default)]
    merge_commit: Option<String>,
}

#[async_trait]
impl Tool for FinalizeTask {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "finalize_task".into(),
            description: "Mark a task in `review` as `done` after it has been squash-merged into \
                 the integration branch. Pass the merge commit sha so the run log can \
                 record it."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string"},
                    "merge_commit": {"type": "string"}
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
        match self
            .handle
            .finalize_task(&args.task_id, args.merge_commit)
            .await
        {
            Ok(()) => ToolOutcome::ok(format!("finalized task {}", args.task_id)),
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    }
}
