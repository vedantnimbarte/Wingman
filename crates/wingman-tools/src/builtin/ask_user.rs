//! `ask_user` — calibrated uncertainty. The agent's way to *ask instead of
//! guess* at a genuine decision fork or before an irreversible action.
//!
//! Most agents guess confidently at forks (which API? which file? delete this?)
//! and users hate the confidently-wrong result. This tool lets the model pause
//! and ask when a wrong guess is costly. When run in an interactive terminal it
//! reads the user's answer from stdin; otherwise it returns a clear note so the
//! model proceeds with its best judgment (and says so).

use crate::{Capability, Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};

#[derive(Debug, Deserialize)]
struct Args {
    /// The question to ask the user.
    question: String,
    /// Optional suggested answers to show.
    #[serde(default)]
    options: Vec<String>,
}

pub struct AskUser;

#[async_trait]
impl Tool for AskUser {
    fn capabilities(&self) -> Capability {
        Capability::NONE
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user".into(),
            description: "Ask the user a question at a genuine decision fork or before an irreversible \
                          action, when you're uncertain and a wrong guess would be costly (which of two \
                          designs, an ambiguous requirement, deleting/overwriting something important). \
                          Do NOT use it for routine choices you can make yourself. Returns the user's \
                          answer, or a note that no interactive answer is available (then proceed with \
                          your best judgment and state the assumption)."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question to ask." },
                    "options": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional suggested answers."
                    }
                },
                "required": ["question"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let a: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let prompt = build_prompt(&a.question, &a.options);

        // Only read stdin when it's an interactive terminal; otherwise a piped/
        // headless run would block or consume unrelated input.
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            return ToolOutcome::ok(format!(
                "(no interactive terminal — could not ask: \"{}\". Proceed with your best judgment \
                 and state the assumption you made.)",
                a.question
            ));
        }

        let answer = tokio::task::spawn_blocking(move || {
            use std::io::{stderr, stdin, Write};
            let _ = write!(stderr(), "{prompt}");
            let _ = stderr().flush();
            let mut line = String::new();
            match stdin().read_line(&mut line) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(line.trim().to_string()),
            }
        })
        .await
        .unwrap_or(None);

        match answer {
            Some(a) if !a.is_empty() => ToolOutcome::ok(format!("user answered: {a}")),
            _ => ToolOutcome::ok(
                "(user gave no answer — proceed with your best judgment and state the assumption)",
            ),
        }
    }
}

fn build_prompt(question: &str, options: &[String]) -> String {
    let mut s = format!("\n\x1b[1m? {question}\x1b[0m\n");
    for (i, o) in options.iter().enumerate() {
        s.push_str(&format!("   {}. {o}\n", i + 1));
    }
    s.push_str("> ");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_question_and_options() {
        let p = build_prompt("Which DB?", &["postgres".into(), "sqlite".into()]);
        assert!(p.contains("Which DB?"));
        assert!(p.contains("1. postgres"));
        assert!(p.contains("2. sqlite"));
    }

    #[tokio::test]
    async fn non_interactive_returns_graceful_note() {
        // In tests stdin isn't a terminal, so it should not block and should
        // return an ok note telling the model to proceed.
        let dir = std::env::temp_dir();
        let ctx = ToolCtx::new(wingman_config::PermissionMode::ReadOnly, dir.clone(), dir);
        let out = AskUser.run(json!({ "question": "Proceed?" }), &ctx).await;
        assert!(!out.is_error);
        assert!(out.content.contains("Proceed") || out.content.contains("best judgment"));
    }
}
