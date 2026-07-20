//! `present_plan`: in Plan mode, the assistant uses this tool to formally
//! present a multi-step plan to the user before making any edits. The tool
//! itself just echoes the plan back to the transcript — the UI is expected
//! to render it as a distinct block and (eventually) gate the
//! Plan→AutoEdit transition on user approval. Outside Plan mode this is a
//! no-op that returns the plan verbatim, so the model can still use it as
//! a structured "what I intend to do" marker.

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};

pub struct PresentPlan;

#[derive(Debug, Deserialize)]
struct Args {
    /// Short title for the plan.
    title: String,
    /// Ordered list of concrete steps the assistant will take.
    steps: Vec<String>,
    /// Optional risk / non-obvious caveats the user should know.
    #[serde(default)]
    caveats: Vec<String>,
}

#[async_trait]
impl Tool for PresentPlan {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "present_plan".into(),
            description:
                "Present a structured implementation plan to the user. In Plan mode this is the \
                 required formal step before any write/shell tool is allowed; the UI surfaces it \
                 distinctly. Outside Plan mode the call still records the plan into the transcript."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short plan title." },
                    "steps": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Ordered concrete steps."
                    },
                    "caveats": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Non-obvious risks / assumptions."
                    }
                },
                "required": ["title", "steps"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        if args.steps.is_empty() {
            return ToolOutcome::err("plan must include at least one step");
        }
        let mut out = String::new();
        out.push_str(&format!("# Plan: {}\n\n", args.title));
        out.push_str("## Steps\n");
        for (i, s) in args.steps.iter().enumerate() {
            out.push_str(&format!("{}. {s}\n", i + 1));
        }
        if !args.caveats.is_empty() {
            out.push_str("\n## Caveats\n");
            for c in &args.caveats {
                out.push_str(&format!("- {c}\n"));
            }
        }
        ToolOutcome::ok(out)
    }
}
