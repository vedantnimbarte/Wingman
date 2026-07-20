//! `save_memory`: persist a fact / preference / instruction the agent has
//! learned about the user or project. Backed by [`wingman_learn::MemoryStore`].

use std::sync::{Arc, Mutex};

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_learn::hooks::LearnSignals;
use wingman_learn::memory::{MemoryDraft, MemoryScope, MemoryStore, MemoryType};

pub struct SaveMemory {
    store: Arc<MemoryStore>,
    signals: Arc<Mutex<LearnSignals>>,
}

impl SaveMemory {
    pub fn new(store: Arc<MemoryStore>, signals: Arc<Mutex<LearnSignals>>) -> Self {
        Self { store, signals }
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    name: String,
    description: String,
    #[serde(rename = "type")]
    mtype: String,
    body: String,
    #[serde(default)]
    scope: Option<String>,
}

#[async_trait]
impl Tool for SaveMemory {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "save_memory".into(),
            description: "Persist a fact, preference, or instruction about the user or project so \
                          future sessions can read it. Use this when the user says \"remember\", \
                          \"from now on\", or expresses a stable preference. \
                          Types: 'user' (about the human), 'feedback' (how to behave), \
                          'project' (about this codebase), 'reference' (pointer to external info). \
                          Scope defaults to 'global' for user/feedback/reference and 'project' for project."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name":        { "type": "string", "description": "Short slug, e.g. 'prefers-terse'." },
                    "description": { "type": "string", "description": "One-line summary used in the prompt index." },
                    "type":        { "type": "string", "enum": ["user", "feedback", "project", "reference"] },
                    "body":        { "type": "string", "description": "Full memory body in markdown." },
                    "scope":       { "type": "string", "enum": ["global", "project"], "description": "Override default scope." }
                },
                "required": ["name", "description", "type", "body"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let mtype = match MemoryType::parse(&args.mtype) {
            Some(t) => t,
            None => {
                return ToolOutcome::err(format!(
                    "unknown memory type '{}': expected one of user|feedback|project|reference",
                    args.mtype
                ))
            }
        };
        let scope = match args.scope.as_deref() {
            None => None,
            Some("global") => Some(MemoryScope::Global),
            Some("project") => Some(MemoryScope::Project),
            Some(other) => {
                return ToolOutcome::err(format!(
                    "unknown scope '{other}': expected 'global' or 'project'"
                ))
            }
        };
        let draft = MemoryDraft {
            name: args.name.clone(),
            description: args.description,
            mtype,
            body: args.body,
            scope,
        };
        match self.store.save(draft) {
            Ok(path) => {
                if let Ok(mut s) = self.signals.lock() {
                    s.saved_this_session = true;
                }
                ToolOutcome::ok(format!(
                    "Saved memory '{}' to {}",
                    args.name,
                    path.display()
                ))
            }
            Err(e) => ToolOutcome::err(format!("save_memory: {e}")),
        }
    }
}
