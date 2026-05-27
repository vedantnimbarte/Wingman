//! `spawn_subagent`: run an isolated inner agent loop with a sub-prompt and
//! return its final assistant text.
//!
//! The runner is provided as a closure at construction time so this crate
//! doesn't have to depend on a specific provider or registry. Recursion is
//! bounded by the runner itself (the closure that builds the inner agent
//! should refuse to register another `spawn_subagent`, so depth caps at 2).

use crate::{Tool, ToolCtx};
use arccode_core::{ToolOutcome, ToolSpec};
use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

/// `prompt`, `description` → final assistant text or an error message.
pub type SubagentRunner =
    Arc<dyn Fn(SubagentSpec) -> BoxFuture<'static, Result<String, String>> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct SubagentSpec {
    pub task: String,
    /// Short orientation prepended to the inner system prompt.
    pub description: String,
    /// Override the model for this subagent (`provider/model`). Empty = use
    /// the parent's selection.
    pub model: String,
}

pub struct SpawnSubagent {
    runner: SubagentRunner,
}

impl SpawnSubagent {
    pub fn new(runner: SubagentRunner) -> Self {
        Self { runner }
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    /// Concrete sub-task for the inner agent to accomplish.
    task: String,
    /// Short description shown to the inner agent ("what you are doing
    /// and why"). Lets the parent shape the subagent's behavior.
    #[serde(default)]
    description: String,
    /// Optional model override (`provider/model`) for the subagent.
    #[serde(default)]
    model: String,
}

#[async_trait]
impl Tool for SpawnSubagent {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "spawn_subagent".into(),
            description: concat!(
                "Run an isolated inner agent loop on a focused sub-task and return its final ",
                "assistant text. The subagent has its own conversation history (no access to ",
                "the parent's). Use this to parallelize research or protect the parent's ",
                "context window from large tool outputs. Cannot nest deeper than one level."
            )
            .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "The exact sub-task prompt." },
                    "description": { "type": "string", "description": "Short orientation for the subagent." },
                    "model": { "type": "string", "description": "Override `provider/model` for the subagent. Empty = inherit." }
                },
                "required": ["task"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let spec = SubagentSpec {
            task: args.task,
            description: args.description,
            model: args.model,
        };
        match (self.runner)(spec).await {
            Ok(text) => ToolOutcome::ok(text),
            Err(e) => ToolOutcome::err(format!("subagent failed: {e}")),
        }
    }
}
