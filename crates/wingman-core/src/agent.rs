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
/// `AgentLoop` doesn't have to depend on `wingman-tools` (which depends on
/// us).
#[async_trait]
pub trait ToolDispatcher: Send + Sync {
    fn specs(&self) -> Vec<ToolSpec>;
    /// Run a single tool call. Stringify any structured output before
    /// returning — the model sees a string.
    async fn dispatch(&self, name: &str, args: serde_json::Value) -> ToolOutcome;
}

/// Hook the agent loop calls at three well-known points so a side-channel
/// crate (`wingman-learn`) can implement the self-improvement loop without
/// `wingman-core` depending on it.
///
/// The default impl is a no-op so existing callers that don't supply a hook
/// pay nothing.
#[async_trait]
pub trait LearningHook: Send + Sync {
    /// Called once before the per-turn provider request. May return extra
    /// system text to splice onto `AgentConfig::system` for this turn only
    /// (memory recall, nudges, ephemeral skill injection, index retrieval).
    /// Async so implementations can hit I/O (e.g. a RAG search) without
    /// blocking the loop's runtime.
    async fn before_turn(&self, _history: &[Message]) -> Option<String> {
        None
    }
    /// Called after each assistant turn completes (tool round trip done).
    fn after_turn(&self, _history: &[Message]) {}
    /// Called once when the loop yields its final Stop event for a user
    /// turn. Use this to flush stats, kick off background indexing, etc.
    fn after_stop(&self, _history: &[Message]) {}
}

/// No-op default — used when the caller doesn't supply a hook.
pub struct NoopLearningHook;
#[async_trait]
impl LearningHook for NoopLearningHook {}

/// Result of one post-edit verification run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateReport {
    pub passed: bool,
    /// Human-readable receipt: command, status, tail of output.
    pub summary: String,
}

/// Post-edit verification gate. When configured on [`AgentConfig`], the loop
/// runs it before accepting an `EndTurn` stop for any user turn in which a
/// mutating tool executed. A failing report is fed back to the model (bounded
/// by `gate_max_retries`) so it can self-correct instead of claiming "done"
/// with broken code.
#[async_trait]
pub trait TurnGate: Send + Sync {
    /// Short label shown in receipts (typically the command line).
    fn label(&self) -> String;
    async fn check(&self) -> GateReport;
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
    /// Result of the post-edit verification gate. Emitted before `Stop`
    /// whenever a gate is configured and mutating tools ran this user turn.
    Verification { passed: bool, summary: String },
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
#[derive(Clone)]
pub struct AgentConfig {
    pub model: String,
    pub system: Option<String>,
    pub max_turns: usize,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    /// Cache after `system` + tools by default. Empty disables explicit caching.
    pub cache_breakpoints: Vec<CacheBreakpoint>,
    /// Roll a cache breakpoint onto the last message each turn so the growing
    /// conversation prefix is cached and subsequent turns read it instead of
    /// re-billing every prior turn. Providers without explicit cache control
    /// ignore it. On by default; only pays off across multi-turn loops.
    pub cache_conversation: bool,
    /// Truncate large tool outputs before feeding them back to the model.
    pub tool_output_budget: ToolOutputBudget,
    /// Compaction policy. Compaction runs **before** each request if the
    /// estimated context size crosses `compactor.trigger_tokens`.
    pub compactor: Compactor,
    /// Optional learning hook. Called at before_turn / after_turn /
    /// after_stop; lets `wingman-learn` inject memory + nudges into the
    /// system prompt and track skill usage outcomes.
    pub learning: Option<Arc<dyn LearningHook>>,
    /// Optional post-edit verification gate (see [`TurnGate`]).
    pub gate: Option<Arc<dyn TurnGate>>,
    /// Gate failures fed back to the model before stopping anyway.
    pub gate_max_retries: usize,
    /// Tool names that count as "mutating" for gate purposes. A successful
    /// call to any of these arms the gate for the rest of the user turn.
    pub mutating_tools: Vec<String>,
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("model", &self.model)
            .field("system", &self.system)
            .field("max_turns", &self.max_turns)
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("cache_breakpoints", &self.cache_breakpoints)
            .field("cache_conversation", &self.cache_conversation)
            .field("tool_output_budget", &self.tool_output_budget)
            .field("compactor", &self.compactor)
            .field("learning", &self.learning.as_ref().map(|_| "<hook>"))
            .field("gate", &self.gate.as_ref().map(|g| g.label()))
            .field("gate_max_retries", &self.gate_max_retries)
            .field("mutating_tools", &self.mutating_tools)
            .finish()
    }
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
            cache_conversation: true,
            tool_output_budget: ToolOutputBudget::default(),
            compactor: Compactor::default(),
            learning: None,
            gate: None,
            gate_max_retries: 2,
            mutating_tools: vec![
                "write_file".into(),
                "edit_file".into(),
                "apply_patch".into(),
                "edit_symbol".into(),
                "run_shell".into(),
            ],
        }
    }
}

pub struct AgentLoop {
    provider: Arc<dyn Provider>,
    tools: Arc<dyn ToolDispatcher>,
    config: AgentConfig,
    /// Conversation history that persists across calls to `run`.
    history: Vec<Message>,
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
        }
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    /// Replace the conversation history — used by the TUI `/resume` command to
    /// reload a saved session's messages into a live agent.
    pub fn set_history(&mut self, history: Vec<Message>) {
        self.history = history;
    }

    /// Force a compaction pass now, regardless of the token threshold: fold
    /// everything but the most recent messages into a single recap. Returns
    /// the number of messages folded, or `None` if history is too short.
    pub fn compact_now(&mut self) -> Option<usize> {
        let plan = self.config.compactor.plan_forced(&self.history)?;
        let replaced = plan.replaced;
        self.history.splice(0..replaced, std::iter::once(plan.recap));
        Some(replaced)
    }

    /// Estimated tokens currently held in history (plus the system prompt).
    pub fn context_tokens(&self) -> u32 {
        crate::tokens::estimate_history_tokens(&self.history, self.config.system.as_deref())
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
        self.history.push(Message::user_text(user_prompt));

        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let config = self.config.clone();
        let history = &mut self.history;

        let stream = async_stream::stream! {
            let specs = tools.specs();
            // Armed when a mutating tool succeeds this user turn; checked by
            // the verification gate before an EndTurn stop is accepted.
            let mut mutated = false;
            let mut gate_attempts: usize = 0;
            for turn in 0..config.max_turns {
                // Compaction pass — fold the oldest non-recap span into a single
                // recap message when we cross the trigger budget.
                if let Some(CompactPlan { recap, replaced }) =
                    config.compactor.plan(history, config.system.as_deref())
                {
                    history.splice(0..replaced, std::iter::once(recap));
                }

                // Allow the learning hook to splice extra system text on a
                // per-turn basis (memory index, nudges, ephemeral skill body).
                let system_for_turn = match (config.system.as_deref(), &config.learning) {
                    (base, Some(hook)) => match hook.before_turn(history).await {
                        Some(extra) if !extra.trim().is_empty() => {
                            let mut s = String::new();
                            if let Some(b) = base {
                                s.push_str(b);
                                if !s.ends_with('\n') {
                                    s.push('\n');
                                }
                                s.push('\n');
                            }
                            s.push_str(&extra);
                            Some(s)
                        }
                        _ => base.map(str::to_string),
                    },
                    (base, None) => base.map(str::to_string),
                };

                // Roll a cache breakpoint onto the last message so this turn's
                // request caches the whole conversation prefix and the next
                // turn reads it. Recomputed per turn because `history` grows.
                let mut cache_breakpoints = config.cache_breakpoints.clone();
                if config.cache_conversation && !history.is_empty() {
                    cache_breakpoints.push(CacheBreakpoint::AfterMessage(history.len() - 1));
                }

                let req = CompletionRequest {
                    model: config.model.clone(),
                    system: system_for_turn,
                    messages: history.clone(),
                    tools: specs.clone(),
                    max_tokens: config.max_tokens,
                    temperature: config.temperature,
                    cache_breakpoints,
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

                if let Some(hook) = &config.learning {
                    hook.after_turn(history);
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

                // If the model said end_turn but emitted tool calls anyway,
                // run them and keep going — this is a provider quirk we
                // observed with some non-Anthropic backends.
                let stop_now = match stop_reason {
                    StopReason::MaxTokens => Some(AgentStop::MaxTokens),
                    _ if tool_calls.is_empty() => Some(AgentStop::EndTurn),
                    _ => None,
                };

                if let Some(reason) = stop_now {
                    // Post-edit verification: when mutating tools ran this
                    // user turn, the gate must pass before an EndTurn stop is
                    // accepted. Failures are fed back to the model (bounded by
                    // gate_max_retries) so it self-corrects instead of
                    // claiming "done" with broken code.
                    if reason == AgentStop::EndTurn && mutated {
                        if let Some(gate) = &config.gate {
                            let report = gate.check().await;
                            yield AgentEvent::Verification {
                                passed: report.passed,
                                summary: report.summary.clone(),
                            };
                            if !report.passed
                                && gate_attempts < config.gate_max_retries
                                && turn + 1 < config.max_turns
                            {
                                gate_attempts += 1;
                                history.push(Message::user_text(format!(
                                    "[wingman verify] Turn gate failed after your edits \
                                     ({}). Fix the issues, then end the turn again.\n\n{}",
                                    gate.label(),
                                    report.summary,
                                )));
                                continue;
                            }
                        }
                    }
                    if let Some(hook) = &config.learning {
                        hook.after_stop(history);
                    }
                    yield AgentEvent::Stop { reason };
                    return;
                }

                // Dispatch tools and append their results as a user-role message.
                let mut results: Vec<ContentBlock> = Vec::with_capacity(tool_calls.len());
                for (id, name, input) in tool_calls {
                    // Always dispatch fresh. A per-turn result cache was removed
                    // because a cached read (`read_file`, `run_shell`) goes stale
                    // the moment a later tool mutates the workspace — which
                    // silently fed the model old output and defeated the
                    // post-edit verification loop.
                    let outcome = tools.dispatch(&name, input).await;
                    if !outcome.is_error && config.mutating_tools.iter().any(|t| t == &name) {
                        mutated = true;
                    }
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
                    if let Some(hook) = &config.learning {
                        hook.after_stop(history);
                    }
                    yield AgentEvent::Stop { reason: AgentStop::MaxTurns };
                    return;
                }
            }
        };

        Box::pin(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderCapabilities, ProviderEventStream};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Provider that replays a scripted sequence of responses, one per
    /// `complete` call.
    struct ScriptedProvider {
        responses: Mutex<VecDeque<Vec<StreamEvent>>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn id(&self) -> &str {
            "scripted"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tools: true,
                vision: false,
                cache_kind: crate::CacheKind::None,
            }
        }
        async fn complete(&self, _req: CompletionRequest) -> crate::Result<ProviderEventStream> {
            let events = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("provider called more times than scripted");
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }
    }

    struct OkDispatcher;
    #[async_trait]
    impl ToolDispatcher for OkDispatcher {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }
        async fn dispatch(&self, _name: &str, _args: serde_json::Value) -> ToolOutcome {
            ToolOutcome::ok("ok")
        }
    }

    /// Provider that records the `cache_breakpoints` of every request it
    /// receives, so tests can assert what the loop asked to be cached.
    struct CapturingProvider {
        captured: Arc<Mutex<Vec<Vec<CacheBreakpoint>>>>,
        responses: Mutex<VecDeque<Vec<StreamEvent>>>,
    }

    #[async_trait]
    impl Provider for CapturingProvider {
        fn id(&self) -> &str {
            "capturing"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tools: true,
                vision: false,
                cache_kind: crate::CacheKind::Explicit,
            }
        }
        async fn complete(&self, req: CompletionRequest) -> crate::Result<ProviderEventStream> {
            self.captured.lock().unwrap().push(req.cache_breakpoints.clone());
            let events = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("provider called more times than scripted");
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }
    }

    /// Gate that fails the first `fail_first` checks, then passes. Counts calls.
    struct CountingGate {
        fail_first: usize,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl TurnGate for CountingGate {
        fn label(&self) -> String {
            "test-gate".into()
        }
        async fn check(&self) -> GateReport {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            GateReport {
                passed: n >= self.fail_first,
                summary: format!("check #{}", n + 1),
            }
        }
    }

    fn tool_use_response() -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolUse {
                block: ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({"path": "x.rs", "content": "fn main() {}"}),
                },
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
            },
        ]
    }

    fn end_turn_response(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta { text: text.into() },
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]
    }

    async fn collect_events(agent: &mut AgentLoop) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        let mut stream = agent.run("do something".into());
        while let Some(ev) = stream.next().await {
            out.push(ev);
        }
        out
    }

    fn capturing_agent(
        responses: Vec<Vec<StreamEvent>>,
        captured: Arc<Mutex<Vec<Vec<CacheBreakpoint>>>>,
        cfg: AgentConfig,
    ) -> AgentLoop {
        let provider = Arc::new(CapturingProvider {
            captured,
            responses: Mutex::new(responses.into()),
        });
        AgentLoop::new(provider, Arc::new(OkDispatcher), cfg)
    }

    #[tokio::test]
    async fn rolling_conversation_cache_breakpoint_is_injected() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut agent = capturing_agent(
            vec![end_turn_response("hi")],
            captured.clone(),
            AgentConfig {
                model: "m".into(),
                ..Default::default()
            },
        );
        let _ = collect_events(&mut agent).await;
        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        // At request time history is just the one user message (index 0).
        assert!(
            reqs[0].contains(&CacheBreakpoint::AfterMessage(0)),
            "rolling breakpoint missing: {:?}",
            reqs[0]
        );
        // Default system/tools breakpoints are preserved alongside it.
        assert!(reqs[0].contains(&CacheBreakpoint::AfterSystem));
    }

    #[tokio::test]
    async fn rolling_cache_breakpoint_absent_when_disabled() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut agent = capturing_agent(
            vec![end_turn_response("hi")],
            captured.clone(),
            AgentConfig {
                model: "m".into(),
                cache_conversation: false,
                ..Default::default()
            },
        );
        let _ = collect_events(&mut agent).await;
        let reqs = captured.lock().unwrap();
        assert!(
            !reqs[0]
                .iter()
                .any(|b| matches!(b, CacheBreakpoint::AfterMessage(_))),
            "breakpoint should be absent when disabled: {:?}",
            reqs[0]
        );
    }

    fn agent_with_gate(responses: Vec<Vec<StreamEvent>>, gate: Arc<CountingGate>) -> AgentLoop {
        AgentLoop::new(
            Arc::new(ScriptedProvider::new(responses)),
            Arc::new(OkDispatcher),
            AgentConfig {
                model: "scripted/test".into(),
                gate: Some(gate),
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn gate_runs_once_and_passes_after_mutation() {
        let gate = Arc::new(CountingGate {
            fail_first: 0,
            calls: AtomicUsize::new(0),
        });
        let mut agent = agent_with_gate(
            vec![tool_use_response(), end_turn_response("done")],
            gate.clone(),
        );
        let events = collect_events(&mut agent).await;

        assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Verification { passed: true, .. })));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Stop {
                reason: AgentStop::EndTurn
            })
        ));
    }

    #[tokio::test]
    async fn gate_failure_feeds_back_then_stop_on_pass() {
        let gate = Arc::new(CountingGate {
            fail_first: 1,
            calls: AtomicUsize::new(0),
        });
        let mut agent = agent_with_gate(
            vec![
                tool_use_response(),
                end_turn_response("done (broken)"),
                end_turn_response("done (fixed)"),
            ],
            gate.clone(),
        );
        let events = collect_events(&mut agent).await;

        assert_eq!(gate.calls.load(Ordering::SeqCst), 2);
        let verifications: Vec<bool> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Verification { passed, .. } => Some(*passed),
                _ => None,
            })
            .collect();
        assert_eq!(verifications, vec![false, true]);
        // The failure was fed back to the model as a user message.
        assert!(agent.history().iter().any(|m| {
            m.role == Role::User
                && m.content.iter().any(|b| {
                    matches!(
                        b,
                        ContentBlock::Text { text } if text.contains("[wingman verify]")
                    )
                })
        }));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Stop {
                reason: AgentStop::EndTurn
            })
        ));
    }

    #[tokio::test]
    async fn gate_retries_exhausted_stops_with_failing_receipt() {
        let gate = Arc::new(CountingGate {
            fail_first: usize::MAX,
            calls: AtomicUsize::new(0),
        });
        let mut agent = AgentLoop::new(
            Arc::new(ScriptedProvider::new(vec![
                tool_use_response(),
                end_turn_response("a"),
                end_turn_response("b"),
            ])),
            Arc::new(OkDispatcher),
            AgentConfig {
                model: "scripted/test".into(),
                gate: Some(gate.clone()),
                gate_max_retries: 1,
                ..Default::default()
            },
        );
        let events = collect_events(&mut agent).await;

        // One failure fed back, second failure accepted: stop anyway.
        assert_eq!(gate.calls.load(Ordering::SeqCst), 2);
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Stop {
                reason: AgentStop::EndTurn
            })
        ));
    }

    #[tokio::test]
    async fn gate_not_run_without_mutation() {
        let gate = Arc::new(CountingGate {
            fail_first: 0,
            calls: AtomicUsize::new(0),
        });
        let mut agent = agent_with_gate(vec![end_turn_response("pure chat")], gate.clone());
        let events = collect_events(&mut agent).await;

        assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::Verification { .. })));
    }
}
