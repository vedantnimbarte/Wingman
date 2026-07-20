//! `update_tasks`: the assistant maintains a live checklist for the current
//! multi-step task. The TUI observes the tool call and renders the latest
//! list in a side panel; each call REPLACES the whole list. The tool itself
//! just returns a short receipt — the value is the visible progress.

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};

pub struct UpdateTasks;

#[derive(Debug, Deserialize)]
struct Args {
    tasks: Vec<TaskArg>,
}

#[derive(Debug, Deserialize)]
struct TaskArg {
    #[allow(dead_code)]
    text: String,
    status: String,
}

#[async_trait]
impl Tool for UpdateTasks {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "update_tasks".into(),
            description:
                "Maintain a visible checklist for the current multi-step task. Call whenever the \
                 plan changes or a step's status changes — each call REPLACES the whole list. Use \
                 it for non-trivial work (roughly 3+ steps); skip it for one-shot requests. Keep \
                 exactly one task 'in_progress' at a time and mark tasks 'done' as you finish them."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "description": "The full checklist, in order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": { "type": "string", "description": "Short task description." },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "done"],
                                    "description": "Current status of this task."
                                }
                            },
                            "required": ["text", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["tasks"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let parsed: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let done = parsed.tasks.iter().filter(|t| t.status == "done").count();
        ToolOutcome::ok(format!(
            "task list updated ({done}/{} done)",
            parsed.tasks.len()
        ))
    }
}
