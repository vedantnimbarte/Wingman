//! `run_acceptance`: worker-only tool that executes the task's acceptance
//! checks and surfaces results back to the model.
//!
//! Workers call this exactly once before `task_complete`. The role
//! prompts (in `prompts/<role>.md`) instruct them to verify every check
//! is green and to include the results in their final `task_complete`
//! payload. Failing acceptance gates the task back to `Failed` for the
//! retry watchdog to pick up (see [`crate::worker::parse_line`] +
//! E3.3 changes in [`crate::worker`]).

use async_trait::async_trait;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_tools::{Tool, ToolCtx};

use crate::acceptance::{run_acceptance_checks, summarize, AcceptanceResult};
use crate::model::Task;

pub struct RunAcceptance;

#[async_trait]
impl Tool for RunAcceptance {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_acceptance".into(),
            description:
                "Run every acceptance check declared on the task (shell commands and grep \
                 patterns). Returns a JSON object with per-check results plus a summary. \
                 Call this AFTER your edits are committed but BEFORE `task_complete`. \
                 Pass the returned results array as `task_complete`'s `acceptance_results` \
                 field. If anything is red, fix the underlying issue and call this tool \
                 again before reporting done."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "Task id; matches the task file under .wingman/pilot/."
                    }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let task_id = match args.get("task_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return ToolOutcome::err("missing `task_id`"),
        };

        let task_path = ctx
            .cwd
            .join(".wingman")
            .join("pilot")
            .join(format!("task-{task_id}.json"));
        let body = match std::fs::read_to_string(&task_path) {
            Ok(b) => b,
            Err(e) => {
                return ToolOutcome::err(format!(
                    "could not read task file {}: {e}",
                    task_path.display()
                ))
            }
        };
        let task: Task = match serde_json::from_str(&body) {
            Ok(t) => t,
            Err(e) => return ToolOutcome::err(format!("task file is not valid JSON: {e}")),
        };

        if task.acceptance.is_empty() {
            return ToolOutcome::ok(
                "no acceptance checks defined — proceed to task_complete with an empty \
                 acceptance_results array",
            );
        }

        let cwd = ctx.cwd.clone();
        let acceptance = task.acceptance.clone();
        let results: Vec<AcceptanceResult> =
            tokio::task::spawn_blocking(move || run_acceptance_checks(&acceptance, &cwd))
                .await
                .unwrap_or_default();

        let summary = summarize(&results);
        let payload = json!({
            "summary": summary,
            "results": results,
        });
        ToolOutcome::ok(payload.to_string())
    }
}
