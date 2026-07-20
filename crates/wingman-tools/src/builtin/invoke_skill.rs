//! `invoke_skill`: look up a named skill and return its body. The agent
//! then internalises the instructions for the current turn. The call is
//! recorded in the skill_usage stats db; the next user reply scores it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_learn::hooks::LearnSignals;
use wingman_learn::stats::StatsStore;

pub struct InvokeSkill {
    project_root: PathBuf,
    stats: Arc<StatsStore>,
    signals: Arc<Mutex<LearnSignals>>,
    session_id: String,
}

impl InvokeSkill {
    pub fn new(
        project_root: PathBuf,
        stats: Arc<StatsStore>,
        signals: Arc<Mutex<LearnSignals>>,
        session_id: String,
    ) -> Self {
        Self {
            project_root,
            stats,
            signals,
            session_id,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    name: String,
}

#[async_trait]
impl Tool for InvokeSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "invoke_skill".into(),
            description: "Load a skill by name and return its instruction body. Apply those \
                          instructions for the remainder of this turn. The system prompt's \
                          'Available skills' section lists what's installed."
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
        let skills = wingman_skills::load_all(&self.project_root);
        let skill = match skills.into_iter().find(|s| s.name == args.name) {
            Some(s) => s,
            None => return ToolOutcome::err(format!("no skill named '{}'", args.name)),
        };

        // Record the invocation. Outcome stays 'unclear' until the next
        // user turn lets the LearnHook score it.
        match self.stats.record_invoke(&skill.name, &self.session_id) {
            Ok(row_id) => {
                if let Ok(mut s) = self.signals.lock() {
                    s.pending_skill_row = Some(row_id);
                }
            }
            Err(e) => tracing::warn!("record_invoke({}): {e}", skill.name),
        }

        ToolOutcome::ok(format!(
            "# Skill: {} ({})\n{}\n\n(Apply the above instructions for the rest of this turn.)",
            skill.name, skill.description, skill.body
        ))
    }
}
