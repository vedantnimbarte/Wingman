//! `invoke_skill`: look up a named skill and return its body. The agent
//! then internalises the instructions for the current turn. The call is
//! recorded in the skill_usage stats db; the next user reply scores it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::{Capability, Tool, ToolCtx};
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
    /// Values for the skill body's `{{placeholders}}`.
    #[serde(default)]
    vars: Option<std::collections::HashMap<String, String>>,
}

#[async_trait]
impl Tool for InvokeSkill {
    fn capabilities(&self) -> Capability {
        Capability::READ
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "invoke_skill".into(),
            description: "Load a skill by name and return its instruction body. Apply those \
                          instructions for the remainder of this turn. The system prompt's \
                          'Available skills' section lists what's installed."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "vars": {
                        "type": "object",
                        "description": "Values for the skill's {{placeholders}}. Call without \
                                        `vars` first — the skill will tell you which ones it needs.",
                        "additionalProperties": { "type": "string" }
                    }
                },
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

        // Substitute `{{placeholders}}`. `extract_vars` and `apply_vars` had
        // no callers, so a skill containing `{{ticket_id}}` was handed to the
        // model verbatim and the documented templating did nothing.
        let required = wingman_skills::extract_vars(&skill.body);
        let supplied = args.vars.unwrap_or_default();
        let missing: Vec<&str> = required
            .iter()
            .filter(|v| !supplied.contains_key(*v))
            .map(|v| v.as_str())
            .collect();

        if !missing.is_empty() {
            return ToolOutcome::err(format!(
                "skill '{}' needs value(s) for: {}. Call invoke_skill again with `vars` \
                 set (ask the user if you don't know them).",
                skill.name,
                missing.join(", ")
            ));
        }

        let body = if supplied.is_empty() {
            skill.body.clone()
        } else {
            wingman_skills::apply_vars(&skill.body, &supplied)
        };

        ToolOutcome::ok(format!(
            "# Skill: {} ({})\n{}\n\n(Apply the above instructions for the rest of this turn.)",
            skill.name, skill.description, body
        ))
    }
}

#[cfg(test)]
mod tests {

    /// `{{placeholders}}` in a skill body were injected verbatim: extract_vars
    /// and apply_vars existed but had no callers.
    #[test]
    fn extract_and_apply_round_trip() {
        let body = "Fix {{ticket_id}} in {{component}} and mention {{ticket_id}} again.";
        let mut vars = std::collections::HashMap::new();

        let needed = wingman_skills::extract_vars(body);
        assert_eq!(
            needed,
            vec!["ticket_id".to_string(), "component".to_string()]
        );

        vars.insert("ticket_id".to_string(), "ENG-42".to_string());
        vars.insert("component".to_string(), "auth".to_string());
        let out = wingman_skills::apply_vars(body, &vars);

        assert!(!out.contains("{{"), "no placeholder should survive: {out}");
        assert_eq!(
            out.matches("ENG-42").count(),
            2,
            "every occurrence substituted"
        );
        assert!(out.contains("auth"));
    }
}
