//! `add_task` — append a new task to the running DAG.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_tools::{Tool, ToolCtx};

use crate::model::{Acceptance, Reversibility, Role};
use crate::orchestrator::{NewTaskSpec, OrchestratorHandle};

pub struct AddTask {
    handle: OrchestratorHandle,
}

impl AddTask {
    pub fn new(handle: OrchestratorHandle) -> Self {
        Self { handle }
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    #[serde(default)]
    id: Option<String>,
    role: Role,
    title: String,
    #[serde(default)]
    goal: String,
    #[serde(default)]
    deps: Vec<String>,
    #[serde(default)]
    writes: Vec<String>,
    #[serde(default)]
    acceptance: Vec<Acceptance>,
    #[serde(default)]
    reversibility: Reversibility,
    #[serde(default)]
    reversibility_reason: Option<String>,
}

#[async_trait]
impl Tool for AddTask {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "add_task".into(),
            description: "Append a new task to the running pilot DAG. Use this when re-planning \
                 mid-run (E5 splitter, surprises from a worker's review). Returns the new \
                 task id."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Optional explicit id; assigned automatically otherwise."},
                    "role": {"type": "string", "enum": ["developer","designer","tester","reviewer","refactorer","merge-fixer"]},
                    "title": {"type": "string"},
                    "goal": {"type": "string"},
                    "deps": {"type": "array", "items": {"type": "string"}},
                    "writes": {"type": "array", "items": {"type": "string"}},
                    "acceptance": {"type": "array", "items": {"type": "object"}},
                    "reversibility": {"type": "string", "enum": ["trivial","hard","irreversible"]},
                    "reversibility_reason": {"type": "string"}
                },
                "required": ["role", "title"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let spec = NewTaskSpec {
            id: args.id,
            role: args.role,
            title: args.title,
            goal: args.goal,
            deps: args.deps,
            writes: args.writes,
            acceptance: args.acceptance,
            reversibility: args.reversibility,
            reversibility_reason: args.reversibility_reason,
        };
        match self.handle.add_task(spec).await {
            Ok(id) => ToolOutcome::ok(format!("added task {id}")),
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    }
}
