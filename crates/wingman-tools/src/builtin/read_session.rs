//! `read_session`: fetch the on-disk JSONL of a specific past session by id.
//! Pairs with `recall_session` (which returns ids).

use std::path::PathBuf;

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};

pub struct ReadSession {
    project_root: PathBuf,
}

impl ReadSession {
    pub fn new(project_root: PathBuf) -> Self {
        Self { project_root }
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    session_id: String,
    #[serde(default)]
    max_chars: Option<u32>,
}

#[async_trait]
impl Tool for ReadSession {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_session".into(),
            description: "Read a past session transcript by id (the id from `recall_session`'s \
                          output). Looks first in the current project's sessions dir."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "max_chars":  { "type": "integer", "minimum": 200, "maximum": 20000, "default": 8000 }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let path = match wingman_learn::session_index::session_path_for(
            &self.project_root,
            &args.session_id,
        ) {
            Some(p) => p,
            None => {
                return ToolOutcome::err(format!(
                    "session '{}' not found in {}/.wingman/sessions",
                    args.session_id,
                    self.project_root.display()
                ))
            }
        };
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(t) => t,
            Err(e) => return ToolOutcome::err(format!("read {}: {e}", path.display())),
        };
        let cap = args.max_chars.unwrap_or(8000).clamp(200, 20_000) as usize;
        let trimmed = if text.chars().count() > cap {
            let mut o: String = text.chars().take(cap).collect();
            o.push_str("\n…(truncated; raise max_chars to see more)");
            o
        } else {
            text
        };
        ToolOutcome::ok(trimmed)
    }
}
