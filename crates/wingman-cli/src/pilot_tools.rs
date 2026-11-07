//! Pilot as chat tools: the front door half of the Router/Worker split.
//!
//! A chat session (TUI, `--print`, `serve`) can hand a goal to a background
//! pilot run, keep talking while it works, check on it, and steer it. The run
//! is a detached process with its own workers and worktrees, so a long job
//! never blocks the conversation. With `[tools].preset = "router"` these are
//! the only way the session can change code: delegation is enforced by what
//! the session can do, not by asking the model nicely.
//!
//! Same plumbing as `wingman mcp-serve`'s pilot tools and `POST …/pilot/runs`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_tools::{Capability, Tool, ToolCtx};

pub fn all() -> Vec<std::sync::Arc<dyn Tool>> {
    vec![
        std::sync::Arc::new(PilotRun),
        std::sync::Arc::new(PilotStatus),
        std::sync::Arc::new(PilotTell),
    ]
}

fn parse<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, ToolOutcome> {
    serde_json::from_value(args).map_err(|e| ToolOutcome::err(format!("invalid args: {e}")))
}

pub struct PilotRun;

#[async_trait]
impl Tool for PilotRun {
    /// It starts workers that edit files and run commands, so it is gated
    /// like the tools those workers will call.
    fn capabilities(&self) -> Capability {
        Capability::WRITE | Capability::SHELL
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "pilot_run".into(),
            description: "Hand a goal to a background pilot run: it plans the goal into tasks, \
                runs worker agents in isolated git worktrees, merges and opens a PR. Returns the \
                run id at once, so keep talking to the user; check on it with pilot_status and \
                steer it with pilot_tell. Use it for any change bigger than a quick edit. \
                Without `yes` the run waits at the plan gate for `wingman pilot approve`."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "goal": { "type": "string", "description": "What to get done, with any context the workers need." },
                    "yes": { "type": "boolean", "description": "Approve the plan automatically." },
                    "plan_only": { "type": "boolean" },
                    "max_usd": { "type": "number" }
                },
                "required": ["goal"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let body: crate::serve::pilot::StartBody = match parse(args) {
            Ok(b) => b,
            Err(e) => return e,
        };
        if body.goal.trim().is_empty() {
            return ToolOutcome::err("a run needs a non-empty `goal`".to_string());
        }
        // The run inherits this session's mode as its ceiling.
        match crate::serve::pilot::spawn_detached_run(&ctx.project_root, &body, ctx.mode()).await {
            Ok(run_id) => ToolOutcome::ok(format!(
                "Started pilot run {run_id} in the background. Tell the user it is running; \
                 check on it with pilot_status when they ask or when you next need it."
            )),
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    }
}

pub struct PilotStatus;

#[derive(Deserialize)]
struct StatusArgs {
    #[serde(default)]
    run_id: Option<String>,
}

#[async_trait]
impl Tool for PilotStatus {
    fn capabilities(&self) -> Capability {
        Capability::READ
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "pilot_status".into(),
            description: "List pilot runs in this project, or with `run_id` return one run's \
                state: status, tasks with outcomes, agents, spend and PR URL."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "run_id": { "type": "string", "description": "Run to inspect; omit to list runs." }
                },
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: StatusArgs = match parse(args) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let root = &ctx.project_root;
        match args.run_id {
            Some(id) => match crate::serve::pilot::run_state(root, &id) {
                Some(state) => {
                    ToolOutcome::ok(serde_json::to_string_pretty(&state).unwrap_or_default())
                }
                None => ToolOutcome::err(format!("no pilot run `{id}` in this project")),
            },
            None => match crate::serve::pilot::runs_json(root) {
                Ok(v) => ToolOutcome::ok(v.to_string()),
                Err(e) => ToolOutcome::err(e),
            },
        }
    }
}

pub struct PilotTell;

#[derive(Deserialize)]
struct TellArgs {
    run_id: String,
    message: String,
    #[serde(default)]
    task: Option<String>,
}

#[async_trait]
impl Tool for PilotTell {
    /// Writes the run's control file, and what it says steers workers that
    /// write code.
    fn capabilities(&self) -> Capability {
        Capability::WRITE
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "pilot_tell".into(),
            description: "Pass a follow-up to a live pilot run's workers (a correction, extra \
                context, a changed requirement) without restarting it. With `task`, only that \
                task's worker hears it; otherwise every active worker does."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "run_id": { "type": "string" },
                    "message": { "type": "string" },
                    "task": { "type": "string", "description": "Task id to address; omit for all active workers." }
                },
                "required": ["run_id", "message"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: TellArgs = match parse(args) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let dir = wingman_autonomous::run_dir(&ctx.project_root, &args.run_id);
        if !crate::serve::pilot::valid_run_id(&args.run_id) || !dir.join("tasks.jsonl").exists() {
            return ToolOutcome::err(format!("no pilot run `{}` in this project", args.run_id));
        }
        let cmd = wingman_autonomous::control::ControlCommand::Tell {
            task: args.task,
            message: args.message,
            reply: false,
        };
        match wingman_autonomous::control::append(&dir, &cmd) {
            Ok(()) => ToolOutcome::ok(format!("Sent to run {}.", args.run_id)),
            Err(e) => ToolOutcome::err(format!("could not reach run {}: {e}", args.run_id)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tell_reaches_the_run_control_file_and_refuses_unknown_runs() {
        let dir = tempfile::tempdir().unwrap();
        let run = wingman_autonomous::run_dir(dir.path(), "r1");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("tasks.jsonl"), "").unwrap();
        let ctx = ToolCtx::new(
            wingman_config::PermissionMode::AutoEdit,
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
        );
        let ok = PilotTell
            .run(json!({"run_id": "r1", "message": "use the v2 API"}), &ctx)
            .await;
        assert!(!ok.is_error, "{}", ok.content);
        let control = std::fs::read_dir(&run)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name() != "tasks.jsonl")
            .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
            .collect::<String>();
        assert!(control.contains("use the v2 API"), "{control}");

        let missing = PilotTell
            .run(json!({"run_id": "nope", "message": "x"}), &ctx)
            .await;
        assert!(missing.is_error);
        let escape = PilotTell
            .run(json!({"run_id": "../r1", "message": "x"}), &ctx)
            .await;
        assert!(escape.is_error, "a run id cannot walk out of the runs dir");
    }
}
