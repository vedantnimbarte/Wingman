//! `task_complete`: a worker's terminal tool call.
//!
//! Workers spawned by the pilot-mode orchestrator call this exactly once at
//! the end of a task. It prints a `task_complete` NDJSON line to stdout
//! (which the parent supervisor parses), then returns a short
//! acknowledgement to the model so the agent loop can EndTurn cleanly.
//!
//! Registered only in worker mode (the registry omits it for normal TUI
//! sessions) — the manager has its own `finalize_task` tool.

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write;
use wingman_core::{ToolOutcome, ToolSpec};

pub struct TaskComplete;

#[derive(Debug, Deserialize)]
struct Args {
    /// One-paragraph human-readable description of what was done.
    summary: String,
    /// Files this task modified, relative to the worktree root.
    #[serde(default)]
    files_changed: Vec<String>,
    /// Optional outcome label — for reviewer tasks, "approve" or "rework".
    #[serde(default)]
    outcome: Option<String>,
    /// Per-check results from `run_acceptance` (E3). Workers must include
    /// this when the task declared acceptance checks. Each entry is the
    /// `AcceptanceResult` JSON shape: { label, ok, output }.
    #[serde(default)]
    acceptance_results: Vec<Value>,
}

#[async_trait]
impl Tool for TaskComplete {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_complete".into(),
            description: "Terminal call for a pilot-mode worker. Reports the final summary of \
                 the task and the files changed. After this call, end your turn — \
                 the orchestrator takes over."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "One-paragraph description of what was done."
                    },
                    "files_changed": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Files modified, relative to the worktree root."
                    },
                    "outcome": {
                        "type": "string",
                        "description": "Optional outcome label, e.g. 'approve' or 'rework' for reviewer tasks."
                    },
                    "acceptance_results": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": {"type": "string"},
                                "ok": {"type": "boolean"},
                                "output": {"type": "string"}
                            },
                            "required": ["label", "ok"]
                        },
                        "description": "Results from `run_acceptance` (E3). Required when the task has acceptance checks."
                    }
                },
                "required": ["summary"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };

        // Emit a single NDJSON line on stdout so the supervisor can capture
        // the terminal outcome without parsing the model's natural-language
        // summary. Stays in-band with `--print --json`'s event stream.
        let line = json!({
            "event": "task_complete",
            "summary": args.summary,
            "files_changed": args.files_changed,
            "outcome": args.outcome,
            "acceptance_results": args.acceptance_results,
        });
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        if writeln!(stdout, "{line}").is_ok() {
            let _ = stdout.flush();
        }

        ToolOutcome::ok(
            "task_complete recorded. End your turn now — the orchestrator will take it from here.",
        )
    }
}
