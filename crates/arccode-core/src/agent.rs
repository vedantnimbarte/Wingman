//! The agent loop.
//!
//! `AgentLoop::run` drives a single user turn to completion: it calls the
//! provider, accumulates streamed text and tool-use blocks, dispatches tools
//! via the [`ToolDispatcher`], and re-invokes the provider until the model
//! emits `Stop::EndTurn` (or we hit `max_turns`).
//!
//! Output is a single stream of `AgentEvent`s. UIs (TUI, headless printer,
//! JSON logger) consume the same stream.

use crate::{
    tokens::{CompactPlan, Compactor, ToolOutputBudget},
    CacheBreakpoint, CompletionRequest, ContentBlock, Message, Provider, Role, StopReason,
    StreamEvent, ToolSpec, Usage,
};
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Abstraction over the registry that actually runs tools. Lives here so
/// `AgentLoop` doesn't have to depend on `arccode-tools` (which depends on
/// us).
#[async_trait]
pub trait ToolDispatcher: Send + Sync {
    fn specs(&self) -> Vec<ToolSpec>;
    /// Run a single tool call. Stringify any structured output before
    /// returning — the model sees a string.
    async fn dispatch(&self, name: &str, args: serde_json::Value) -> ToolOutcome;
}

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Streaming text from the assistant.
    TextDelta { text: String },
    /// A tool call about to execute.
    ToolStart {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// The result of a tool call.
    ToolResult {
        id: String,
        output: String,
        is_error: bool,
    },
    /// Usage update (cumulative for the current turn).
    Usage { usage: Usage },
    /// A single provider response finished (one turn-step).
    TurnComplete,
    /// The whole user-turn finished.
    Stop { reason: AgentStop },
    /// Recoverable error surfaced to the UI.
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStop {
    EndTurn,
    MaxTurns,
    MaxTokens,
    Error,
}

/// Construction-time options for the loop.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    pub system: Option<String>,
    pub max_turns: usize,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    /// Cache after `system` + tools by default. Empty disables explicit caching.
    pub cache_breakpoints: Vec<CacheBreakpoint>,
    /// Truncate large tool outputs before feeding them back to the model.
    pub tool_output_budget: ToolOutputBudget,
    /// Compaction policy. Compaction runs **before** each request if the
    /// estimated context size crosses `compactor.trigger_tokens`.
    pub compactor: Compactor,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            system: None,
            max_turns: 16,
            max_tokens: 4096,
            temperature: None,
            cache_breakpoints: vec![CacheBreakpoint::AfterSystem, CacheBreakpoint::AfterTools],
            tool_output_budget: ToolOutputBudget::default(),
            compactor: Compactor::default(),
        }
    }
}

pub struct AgentLoop {
    provider: Arc<dyn Provider>,
    tools: Arc<dyn ToolDispatcher>,
    config: AgentConfig,
    /// Conversation history that persists across calls to `run`.
    history: Vec<Message>,
    /// Per-turn tool output cache. Keyed by (tool_name, canonical_json_args).
    /// Cleared at the start of each call to `run`.
    tool_cache: std::collections::HashMap<(String, String), ToolOutcome>,
}

impl AgentLoop {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<dyn ToolDispatcher>,
        config: AgentConfig,
    ) -> Self {
        Self {
            provider,
            tools,
            config,
            history: Vec::new(),
            tool_cache: Default::default(),
        }
    }

    /// Construct an `AgentLoop` with pre-loaded conversation history, useful
    /// for resuming a previous session via session records.
    pub fn with_history(
        provider: Arc<dyn Provider>,
        tools: Arc<dyn ToolDispatcher>,
        config: AgentConfig,
        history: Vec<Message>,
    ) -> Self {
        Self {
            provider,
            tools,
            config,
            history,
            tool_cache: Default::default(),
        }
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    /// Swap in a different provider. Conversation history is preserved so
    /// the new model picks up mid-stream — providers translate `Message`s
    /// through their own adapter on the next request.
    pub fn swap_provider(&mut self, provider: Arc<dyn Provider>) {
        self.provider = provider;
    }

    pub fn set_model(&mut self, model: impl Into<String>) {
        self.config.model = model.into();
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub fn set_temperature(&mut self, t: Option<f32>) {
        self.config.temperature = t;
    }

    pub fn get_temperature(&self) -> Option<f32> {
        self.config.temperature
    }

    pub fn set_max_tokens(&mut self, n: u32) {
        self.config.max_tokens = n;
    }

    pub fn get_max_tokens(&self) -> u32 {
        self.config.max_tokens
    }

    pub fn get_model(&self) -> &str {
        &self.config.model
    }

    /// Drive a single user turn to completion. The returned stream yields
    /// events live and terminates after a `Stop` event.
    pub fn run(&mut self, user_prompt: String) -> BoxStream<'_, AgentEvent> {
        // Clear the per-turn tool cache at the start of each new user turn.
        self.tool_cache.clear();
        self.history.push(Message::user_text(user_prompt));

        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let config = self.config.clone();
        let history = &mut self.history;
        let tool_cache = &mut self.tool_cache;

        let stream = async_stream::stream! {
            let specs = tools.specs();
            for turn in 0..config.max_turns {
                // Compaction pass — fold the oldest non-recap span into a single
                // recap message when we cross the trigger budget.
                if let Some(CompactPlan { recap, replaced }) =
                    config.compactor.plan(history, config.system.as_deref())
                {
                    history.splice(0..replaced, std::iter::once(recap));
                }

                let req = CompletionRequest {
                    model: config.model.clone(),
                    system: config.system.clone(),
                    messages: history.clone(),
                    tools: specs.clone(),
                    max_tokens: config.max_tokens,
                    temperature: config.temperature,
                    cache_breakpoints: config.cache_breakpoints.clone(),
                };

                let mut event_stream = match provider.complete(req).await {
                    Ok(s) => s,
                    Err(e) => {
                        yield AgentEvent::Error { message: e.to_string() };
                        yield AgentEvent::Stop { reason: AgentStop::Error };
                        return;
                    }
                };

                let mut assistant_blocks: Vec<ContentBlock> = Vec::new();
                let mut current_text = String::new();
                let mut stop_reason: StopReason = StopReason::EndTurn;

                while let Some(evt) = event_stream.next().await {
                    let evt = match evt {
                        Ok(e) => e,
                        Err(e) => {
                            yield AgentEvent::Error { message: e.to_string() };
                            yield AgentEvent::Stop { reason: AgentStop::Error };
                            return;
                        }
                    };
                    match evt {
                        StreamEvent::TextDelta { text } => {
                            current_text.push_str(&text);
                            yield AgentEvent::TextDelta { text };
                        }
                        StreamEvent::ToolUse { block } => {
                            // Flush any pending text into its own block.
                            if !current_text.is_empty() {
                                assistant_blocks.push(ContentBlock::text(std::mem::take(&mut current_text)));
                            }
                            if let ContentBlock::ToolUse { id, name, input } = &block {
                                yield AgentEvent::ToolStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                };
                            }
                            assistant_blocks.push(block);
                        }
                        StreamEvent::Usage { usage } => {
                            yield AgentEvent::Usage { usage };
                        }
                        StreamEvent::Stop { reason } => {
                            stop_reason = reason;
                        }
                    }
                }

                if !current_text.is_empty() {
                    assistant_blocks.push(ContentBlock::text(std::mem::take(&mut current_text)));
                }

                // Persist the assistant turn.
                if !assistant_blocks.is_empty() {
                    history.push(Message {
                        role: Role::Assistant,
                        content: assistant_blocks.clone(),
                    });
                }

                yield AgentEvent::TurnComplete;

                // Decide whether to continue.
                let tool_calls: Vec<(String, String, serde_json::Value)> = assistant_blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, input } => {
                            Some((id.clone(), name.clone(), input.clone()))
                        }
                        _ => None,
                    })
                    .collect();

                match stop_reason {
                    StopReason::EndTurn => {
                        // If the model said end_turn but emitted tool calls anyway,
                        // run them and keep going — this is a provider quirk we
                        // observed with some non-Anthropic backends.
                        if tool_calls.is_empty() {
                            yield AgentEvent::Stop { reason: AgentStop::EndTurn };
                            return;
                        }
                    }
                    StopReason::MaxTokens => {
                        yield AgentEvent::Stop { reason: AgentStop::MaxTokens };
                        return;
                    }
                    StopReason::ToolUse | StopReason::StopSequence | StopReason::Other => {
                        if tool_calls.is_empty() {
                            yield AgentEvent::Stop { reason: AgentStop::EndTurn };
                            return;
                        }
                    }
                }

                // Dispatch tools and append their results as a user-role message.
                let mut results: Vec<ContentBlock> = Vec::with_capacity(tool_calls.len());
                for (id, name, input) in tool_calls {
                    let cache_key = (name.clone(), serde_json::to_string(&input).unwrap_or_default());
                    let outcome = if let Some(cached) = tool_cache.get(&cache_key) {
                        // Cache hit: reuse the previous result without re-dispatching.
                        cached.clone()
                    } else {
                        let fresh = tools.dispatch(&name, input).await;
                        tool_cache.insert(cache_key, fresh.clone());
                        fresh
                    };
                    let truncated = config.tool_output_budget.trim(&outcome.content);
                    // UIs see the *full* output so the user can scroll/copy;
                    // the *model* only sees the truncated version below.
                    yield AgentEvent::ToolResult {
                        id: id.clone(),
                        output: outcome.content,
                        is_error: outcome.is_error,
                    };
                    results.push(ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: truncated,
                        is_error: outcome.is_error,
                    });
                }
                history.push(Message::tool_results(results));

                if turn + 1 == config.max_turns {
                    yield AgentEvent::Stop { reason: AgentStop::MaxTurns };
                    return;
                }
            }
        };

        Box::pin(stream)
    }
}
