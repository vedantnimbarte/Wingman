//! `forget_memory`: delete a stored memory by slug.

use std::sync::Arc;

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_learn::memory::MemoryStore;

pub struct ForgetMemory {
    store: Arc<MemoryStore>,
}

impl ForgetMemory {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    name: String,
}

#[async_trait]
impl Tool for ForgetMemory {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "forget_memory".into(),
            description: "Delete a stored memory by slug. Use only when the user explicitly \
                          asks to forget something or when a memory is clearly wrong."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        match self.store.forget(&args.name) {
            Ok(true) => ToolOutcome::ok(format!("Forgot '{}'", args.name)),
            Ok(false) => ToolOutcome::err(format!("no memory named '{}'", args.name)),
            Err(e) => ToolOutcome::err(format!("forget_memory: {e}")),
        }
    }
}
