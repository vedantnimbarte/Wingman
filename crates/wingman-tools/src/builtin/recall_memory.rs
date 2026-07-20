//! `recall_memory`: fetch the full body of a stored memory by slug, or list
//! all memories when no name is given.

use std::sync::Arc;

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_learn::memory::MemoryStore;

pub struct RecallMemory {
    store: Arc<MemoryStore>,
}

impl RecallMemory {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[derive(Debug, Deserialize, Default)]
struct Args {
    #[serde(default)]
    name: Option<String>,
}

#[async_trait]
impl Tool for RecallMemory {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "recall_memory".into(),
            description: "Read a memory's full body by slug. Omit `name` to list all known \
                          memories. Use this when the system-prompt memory index hints at \
                          something that's relevant to the current task but you need the detail."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Memory slug from the index." }
                },
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = serde_json::from_value(args).unwrap_or_default();
        match args.name {
            Some(name) => match self.store.find(&name) {
                Some(m) => ToolOutcome::ok(format!(
                    "name: {}\ntype: {}\nscope: {}\ndescription: {}\n---\n{}",
                    m.name,
                    m.mtype.as_str(),
                    m.scope.label(),
                    m.description,
                    m.body
                )),
                None => ToolOutcome::err(format!("no memory with name '{name}'")),
            },
            None => {
                let mems = self.store.load_all();
                if mems.is_empty() {
                    return ToolOutcome::ok("(no memories yet — use save_memory to persist one)");
                }
                let mut out = String::new();
                for m in mems {
                    out.push_str(&format!(
                        "- [{}] {} ({}) — {}\n",
                        m.mtype.as_str(),
                        m.name,
                        m.scope.label(),
                        m.description
                    ));
                }
                ToolOutcome::ok(out)
            }
        }
    }
}
