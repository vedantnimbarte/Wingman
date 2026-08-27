//! `job_output`, `job_stop`, `job_list` — control for background shell jobs.
//!
//! Started by `run_shell` with `background: true`. See [`crate::jobs`].

use crate::{Capability, Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};

#[derive(Debug, Deserialize)]
struct IdArgs {
    id: String,
}

pub struct JobOutput;

#[async_trait]
impl Tool for JobOutput {
    fn capabilities(&self) -> Capability {
        // Reading what a job printed is a read, not a new shell execution:
        // the command was gated when it was started, and re-gating here would
        // mean a mode change could strand a running job's output.
        Capability::READ
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "job_output".into(),
            description: "Read the output of a background job started by `run_shell` with \
                          `background: true`. Returns everything buffered so far plus the \
                          job's state; safe to call repeatedly while it runs."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Job id, e.g. `job-1`." }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: IdArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        match ctx.jobs.output(&args.id) {
            Some((text, state)) => {
                let body = format!("[{}] {}\n{text}", args.id, state.label());
                ToolOutcome::ok(body)
            }
            None => ToolOutcome::err(format!(
                "no such job: {} (use job_list to see what is running)",
                args.id
            )),
        }
    }
}

pub struct JobStop;

#[async_trait]
impl Tool for JobStop {
    fn capabilities(&self) -> Capability {
        Capability::SHELL
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "job_stop".into(),
            description: "Stop a background job and its whole process tree. Stopping an \
                          already-finished job is not an error."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Job id, e.g. `job-1`." }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: IdArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        match ctx.jobs.stop(&args.id) {
            Some(_) => ToolOutcome::ok(format!("stopped {} and its process tree", args.id)),
            None => ToolOutcome::err(format!("no such job: {}", args.id)),
        }
    }
}

pub struct JobList;

#[async_trait]
impl Tool for JobList {
    fn capabilities(&self) -> Capability {
        Capability::READ
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "job_list".into(),
            description: "List background jobs for this session with their state and how long \
                          they have been running."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, _args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let rows = ctx.jobs.list();
        if rows.is_empty() {
            return ToolOutcome::ok("(no background jobs)".to_string());
        }
        ToolOutcome::ok(rows.join("\n"))
    }
}
