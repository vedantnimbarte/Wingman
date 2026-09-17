//! `claude-code`: the user's Claude Code CLI as a chat provider.
//!
//! Lets a Claude subscription drive an ordinary Wingman session (TUI,
//! `--print`, `serve`) with no API key. See [`wingman_core::claude_code`] for
//! why this runs the CLI rather than calling the API.
//!
//! Claude Code runs its own tools, so from the agent loop's side every turn is
//! plain text: the CLI's tool calls are narrated inline and nothing comes back
//! as a Wingman tool call. Wingman's tools, gates and permission prompts do not
//! apply here — the CLI's `permission_mode` does. Pilot mode is different: its
//! manager and workers drive [`ClaudeCode`] directly and keep Wingman's tools.
//!
//! Requests without tools (titles, summaries, the pilot's planner and
//! reviewer) run as one-shot completions with the CLI's tools switched off.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::Mutex;
use wingman_core::claude_code::{ClaudeCode, PROVIDER_ID};
use wingman_core::{
    AgentEvent, AgentStop, CacheKind, CompletionRequest, ContentBlock, Message, Provider,
    ProviderCapabilities, ProviderEventStream, Result, Role, StopReason, StreamEvent, WingmanError,
};

pub struct ClaudeCodeProvider {
    permission_mode: String,
    /// The CLI session the conversation lives in, and the history length it
    /// covers. A request that extends exactly that history resumes it and
    /// sends only the new message; anything else (a rewind, a compaction, a
    /// fresh process) starts a new session from the transcript.
    session: Arc<Mutex<(Option<String>, usize)>>,
}

impl ClaudeCodeProvider {
    /// `permission_mode` is the CLI's (`default`, `acceptEdits`, `plan`,
    /// `bypassPermissions`). In `default`, anything that would prompt is denied.
    pub fn new(permission_mode: impl Into<String>) -> Self {
        Self {
            permission_mode: permission_mode.into(),
            session: Arc::new(Mutex::new((None, 0))),
        }
    }
}

#[async_trait]
impl Provider for ClaudeCodeProvider {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tools: false,
            vision: false,
            cache_kind: CacheKind::Automatic,
            reasoning: false,
        }
    }

    async fn complete(&self, req: CompletionRequest) -> Result<ProviderEventStream> {
        let cwd = std::env::current_dir().map_err(|e| WingmanError::Provider(e.to_string()))?;
        let mut cc = ClaudeCode::new(req.model.clone(), cwd);
        let one_shot = req.tools.is_empty();
        let mut resumed = false;
        if one_shot {
            cc.system = req.system.clone();
            cc.tools = Some(String::new());
            cc.max_turns = Some(1);
            cc.ephemeral = true;
        } else {
            // Wingman's system prompt describes Wingman's tools, which the CLI
            // does not have. Its own prompt (and CLAUDE.md) is the right one.
            cc.permission_mode = self.permission_mode.clone();
            cc.resume = true;
            let (id, covered) = self.session.lock().await.clone();
            if id.is_some() && req.messages.len() == covered + 1 {
                cc.set_session_id(id);
                resumed = true;
            }
        }
        let prompt = if resumed {
            req.messages.last().map(text_of).unwrap_or_default()
        } else {
            transcript(&req.messages)
        };

        let session = self.session.clone();
        let len = req.messages.len();
        Ok(Box::pin(async_stream::stream! {
            let mut events = cc.run(prompt);
            let mut stop = StopReason::EndTurn;
            while let Some(ev) = events.next().await {
                match ev {
                    AgentEvent::TextDelta { text } => yield Ok(StreamEvent::TextDelta { text }),
                    AgentEvent::ThinkingDelta { text } => yield Ok(StreamEvent::ThinkingDelta { text }),
                    AgentEvent::ToolStart { name, input, .. } => {
                        yield Ok(StreamEvent::TextDelta { text: format!("\n\n> {name} {}\n\n", brief(&input)) })
                    }
                    AgentEvent::Usage { usage } => yield Ok(StreamEvent::Usage { usage }),
                    AgentEvent::Error { message } => yield Err(WingmanError::Provider(message)),
                    AgentEvent::Stop { reason } => {
                        if reason == AgentStop::MaxTurns {
                            stop = StopReason::MaxTokens;
                        }
                        break;
                    }
                    _ => {}
                }
            }
            drop(events);
            if !one_shot {
                // The loop appends our reply, so the next request extends the
                // history by that plus one user message.
                *session.lock().await = (cc.session_id().map(String::from), len + 1);
            }
            yield Ok(StreamEvent::Stop { reason: stop });
        }))
    }
}

fn text_of(m: &Message) -> String {
    m.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The whole conversation as one prompt, for a fresh session.
fn transcript(messages: &[Message]) -> String {
    if let [only] = messages {
        return text_of(only);
    }
    let mut s = String::from("Continue this conversation. Earlier turns:\n\n");
    for m in &messages[..messages.len().saturating_sub(1)] {
        let who = match m.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
        };
        s.push_str(&format!("{who}: {}\n\n", text_of(m)));
    }
    s.push_str("Now respond to:\n\n");
    s.push_str(&messages.last().map(text_of).unwrap_or_default());
    s
}

/// One line saying what a tool call touched.
fn brief(input: &serde_json::Value) -> String {
    [
        "file_path",
        "command",
        "pattern",
        "path",
        "url",
        "description",
    ]
    .iter()
    .find_map(|k| input.get(k).and_then(|v| v.as_str()))
    .map(|v| v.lines().next().unwrap_or("").chars().take(120).collect())
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_session_gets_the_whole_conversation() {
        let msgs = vec![
            Message::user_text("add a flag".to_string()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::text("done")],
            },
            Message::user_text("now test it".to_string()),
        ];
        let t = transcript(&msgs);
        assert!(t.contains("User: add a flag") && t.contains("Assistant: done"));
        assert!(t.ends_with("now test it"));
        assert_eq!(transcript(&msgs[..1]), "add a flag");
        assert_eq!(
            brief(&serde_json::json!({"command": "cargo test\nmore"})),
            "cargo test"
        );
    }
}
