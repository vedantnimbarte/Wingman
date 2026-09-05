//! `ask_user` — calibrated uncertainty. The agent's way to *ask instead of
//! guess* at a genuine decision fork or before an irreversible action.
//!
//! Most agents guess confidently at forks (which API? which file? delete this?)
//! and users hate the confidently-wrong result. This tool lets the model pause
//! and ask when a wrong guess is costly.
//!
//! Three routes, tried in order:
//!
//! 1. The desktop popup, when `[tools].ask_user_desktop_timeout_secs` is
//!    non-zero *and* the app is actually running. This reaches a detached pilot
//!    run, a worker, a `serve` turn and the TUI alike.
//! 2. stdin, when it is an interactive terminal.
//! 3. Neither — return a note so the model proceeds with its best judgment and
//!    says so.
//!
//! The default is `0`, which skips straight past route 1 and leaves the tool
//! behaving exactly as it always has.

use std::path::Path;
use std::time::Duration;

use crate::{Capability, Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_config::inbox;
use wingman_core::{ToolOutcome, ToolSpec};

/// What the model is told when a question went out and came back unanswered.
const NO_ANSWER: &str =
    "(user gave no answer — proceed with your best judgment and state the assumption)";

/// How often the replies file is checked while waiting. Matches `pilot ask`'s
/// cadence; fast enough that a click feels immediate, slow enough to be free.
const REPLY_POLL: Duration = Duration::from_millis(400);

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

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let a: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };

        // Route 1: the desktop popup, when it is configured *and* up.
        //
        // Deliberately ahead of the stdin check rather than behind it. The TUI
        // holds the terminal in raw mode under an alternate screen, so
        // `stdin().is_terminal()` is true there while a blocking `read_line`
        // fights crossterm for keystrokes and writes its prompt to a stderr
        // nobody can see. Trying the popup first fixes that case as well as the
        // headless one.
        //
        // The liveness check is what keeps this honest: with the feature on but
        // the app closed, fall through to the terminal prompt instead of
        // sitting out a timeout nobody is going to answer.
        if ctx.ask_user_desktop_timeout_secs > 0 && inbox::notifier_alive() {
            if let Ok(dir) = wingman_config::global_dir() {
                let project = ctx
                    .project_root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned());
                let answer = ask_desktop(
                    &dir,
                    &a.question,
                    &a.options,
                    project,
                    Duration::from_secs(ctx.ask_user_desktop_timeout_secs),
                )
                .await;
                return match answer {
                    Some(ans) => ToolOutcome::ok(format!("user answered: {ans}")),
                    None => ToolOutcome::ok(NO_ANSWER),
                };
            }
        }

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
            _ => ToolOutcome::ok(NO_ANSWER),
        }
    }
}

/// Post the question to the desktop inbox and wait up to `timeout` for a reply.
///
/// `None` covers every way this can come back empty — the append failed, the
/// deadline passed, the user dismissed the card — because they all mean the
/// same thing to the model: nobody answered, carry on and state the assumption.
/// It never returns an error: a broken inbox must degrade the tool, not fail it.
async fn ask_desktop(
    dir: &Path,
    question: &str,
    options: &[String],
    project: Option<String>,
    timeout: Duration,
) -> Option<String> {
    // Before the append, not after: a reply that lands in the gap would
    // otherwise be behind the reader's starting offset and never seen.
    let mut rx = inbox::ReplyReader::at_end_in(dir);

    let n = inbox::Notification {
        project,
        expires_at: Some(inbox::now_secs().saturating_add(timeout.as_secs())),
        actions: options
            .iter()
            .map(|o| inbox::Action {
                id: o.clone(),
                label: o.clone(),
                control: None,
            })
            .collect(),
        // Suggested answers are a shortcut, never the whole menu — the model
        // asks open questions too, and one it did not think of is often the
        // right answer.
        free_text: true,
        ..inbox::Notification::now("decision", "wingman is asking", question)
    };
    if inbox::append_to(dir, &n).is_err() {
        return None;
    }

    let id = n.id;
    tokio::time::timeout(timeout, async move {
        loop {
            tokio::time::sleep(REPLY_POLL).await;
            if let Some(r) = rx.poll().into_iter().find(|r| r.id == id) {
                return r.answer().map(str::to_string);
            }
        }
    })
    .await
    .ok()
    .flatten()
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
        // return an ok note telling the model to proceed. The desktop route
        // defaults to off, so this is also the no-regression assertion: the
        // tool behaves exactly as it did before the inbox existed.
        let dir = std::env::temp_dir();
        let ctx = ToolCtx::new(wingman_config::PermissionMode::ReadOnly, dir.clone(), dir);
        assert_eq!(ctx.ask_user_desktop_timeout_secs, 0, "off by default");
        let out = AskUser.run(json!({ "question": "Proceed?" }), &ctx).await;
        assert!(!out.is_error);
        assert!(out.content.contains("Proceed") || out.content.contains("best judgment"));
    }

    #[tokio::test]
    async fn desktop_ask_returns_the_reply() {
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().to_path_buf();

        // Stand in for the desktop app: wait for the card, then answer it.
        let app = tokio::spawn(async move {
            loop {
                if let Some(n) = inbox::read_open(&watched).first() {
                    inbox::append_reply_to(
                        &watched,
                        &inbox::Reply {
                            id: n.id.clone(),
                            action: Some("sqlite".into()),
                            text: None,
                        },
                    )
                    .unwrap();
                    return n.clone();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let got = ask_desktop(
            dir.path(),
            "Postgres or SQLite?",
            &["postgres".into(), "sqlite".into()],
            Some("wingman".into()),
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(got.as_deref(), Some("sqlite"));

        // The card the app saw carries the question, both suggestions as
        // buttons, and a free-text box for an answer neither option covers.
        let card = app.await.unwrap();
        assert_eq!(card.body, "Postgres or SQLite?");
        assert_eq!(card.severity, "decision");
        let labels: Vec<&str> = card.actions.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(labels, ["postgres", "sqlite"]);
        assert!(card.free_text);
        assert!(
            card.actions.iter().all(|a| a.control.is_none()),
            "a question's buttons are answers, not control commands"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn desktop_ask_times_out_rather_than_blocking_a_run() {
        // Paused clock: the deadline passes without the test waiting it out.
        let dir = tempfile::tempdir().unwrap();
        let got = ask_desktop(dir.path(), "Which DB?", &[], None, Duration::from_secs(120)).await;
        assert_eq!(got, None, "nobody answered");
    }

    #[tokio::test]
    async fn desktop_ask_ignores_a_reply_meant_for_an_earlier_question() {
        let dir = tempfile::tempdir().unwrap();
        // An answer to something else, already on file before we ask.
        inbox::append_reply_to(
            dir.path(),
            &inbox::Reply {
                id: "someone-else".into(),
                text: Some("stale".into()),
                action: None,
            },
        )
        .unwrap();

        let got = tokio::time::timeout(
            Duration::from_secs(3),
            ask_desktop(dir.path(), "Which DB?", &[], None, Duration::from_secs(1)),
        )
        .await
        .expect("must not hang");
        assert_eq!(got, None, "a stale reply must not satisfy a new question");
    }
}
