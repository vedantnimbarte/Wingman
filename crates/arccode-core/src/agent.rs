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

/// Hook the agent loop calls at three well-known points so a side-channel
/// crate (`arccode-learn`) can implement the self-improvement loop without
/// `arccode-core` depending on it.
///
/// The default impl is a no-op so existing callers that don't supply a hook
/// pay nothing.
pub trait LearningHook: Send + Sync {
    /// Called once before the per-turn provider request. May return extra
    /// system text to splice onto `AgentConfig::system` for this turn only
    /// (memory recall, nudges, ephemeral skill injection).
    fn before_turn(&self, _history: &[Message]) -> Option<String> {
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
    /// The session's estimated spend crossed `AgentConfig::budget_usd`.
    Budget,
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
    /// Truncate large tool outputs before feeding them back to the model.
    pub tool_output_budget: ToolOutputBudget,
    /// Compaction policy. Compaction runs **before** each request if the
    /// estimated context size crosses `compactor.trigger_tokens`.
    pub compactor: Compactor,
    /// Optional learning hook. Called at before_turn / after_turn /
    /// after_stop; lets `arccode-learn` inject memory + nudges into the
    /// system prompt and track skill usage outcomes.
    pub learning: Option<Arc<dyn LearningHook>>,
    /// Optional post-edit verification gate (see [`TurnGate`]).
    pub gate: Option<Arc<dyn TurnGate>>,
    /// Gate failures fed back to the model before stopping anyway.
    pub gate_max_retries: usize,
    /// Tool names that count as "mutating" for gate purposes. A successful
    /// call to any of these arms the gate for the rest of the user turn.
    pub mutating_tools: Vec<String>,
    /// Hard USD ceiling for the session (estimated from the static pricing
    /// table). `None` = unlimited. Checked before each provider call;
    /// crossing it stops the loop with `AgentStop::Budget`.
    pub budget_usd: Option<f64>,
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
            .field("tool_output_budget", &self.tool_output_budget)
            .field("compactor", &self.compactor)
            .field("learning", &self.learning.as_ref().map(|_| "<hook>"))
            .field("gate", &self.gate.as_ref().map(|g| g.label()))
            .field("gate_max_retries", &self.gate_max_retries)
            .field("mutating_tools", &self.mutating_tools)
            .field("budget_usd", &self.budget_usd)
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
            tool_output_budget: ToolOutputBudget::default(),
            compactor: Compactor::default(),
            learning: None,
            gate: None,
            gate_max_retries: 2,
            budget_usd: None,
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
    /// Per-turn tool output cache. Keyed by (tool_name, canonical_json_args).
    /// Cleared at the start of each call to `run`.
    tool_cache: std::collections::HashMap<(String, String), ToolOutcome>,
    /// Estimated cumulative USD spend for this session (across `run` calls),
    /// from the static pricing table. Unknown models accrue 0.
    spent_usd: f64,
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
            spent_usd: 0.0,
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
            spent_usd: 0.0,
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

    /// Estimated cumulative session spend in USD (static pricing table;
    /// unknown/local models accrue 0).
    pub fn spent_usd(&self) -> f64 {
        self.spent_usd
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
        let spent_usd = &mut self.spent_usd;

        let stream = async_stream::stream! {
            let specs = tools.specs();
            // Armed when a mutating tool succeeds this user turn; checked by
            // the verification gate before an EndTurn stop is accepted.
            let mut mutated = false;
            let mut gate_attempts: usize = 0;
            for turn in 0..config.max_turns {
                // Budget check — refuse to issue another provider call once
                // the session's estimated spend has crossed the ceiling.
                if let Some(limit) = config.budget_usd {
                    if *spent_usd >= limit {
                        yield AgentEvent::Error {
                            message: format!(
                                "session budget exhausted: ~${:.2} spent of ${:.2} limit \
                                 ([budget] max_usd_per_session)",
                                *spent_usd, limit
                            ),
                        };
                        yield AgentEvent::Stop { reason: AgentStop::Budget };
                        return;
                    }
                }

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
                    (base, Some(hook)) => match hook.before_turn(history) {
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

                let req = CompletionRequest {
                    model: config.model.clone(),
                    system: system_for_turn,
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
                let mut turn_usage: Option<Usage> = None;

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
                            // Usage is cumulative within a provider response;
                            // keep the last snapshot for cost accounting.
                            turn_usage = Some(usage.clone());
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

                if let (Some(usage), Some(price)) =
                    (&turn_usage, crate::pricing::price_for(&config.model))
                {
                    *spent_usd += price.cost(usage);
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
                                    "[arccode verify] Turn gate failed after your edits \
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
                //
                // When every call in the batch is read-only (none in
                // `mutating_tools`), misses run concurrently — a model that
                // emits three reads/greps in one turn shouldn't pay for them
                // serially. Batches containing a mutating call keep strict
                // sequential order, since write/shell effects can depend on
                // earlier calls in the same batch.
                let all_readonly = tool_calls
                    .iter()
                    .all(|(_, name, _)| !config.mutating_tools.iter().any(|t| t == name));
                let mut outcomes: Vec<Option<ToolOutcome>> = vec![None; tool_calls.len()];
                if all_readonly && tool_calls.len() > 1 {
                    let mut pending = Vec::new();
                    for (i, (_, name, input)) in tool_calls.iter().enumerate() {
                        let cache_key =
                            (name.clone(), serde_json::to_string(input).unwrap_or_default());
                        if let Some(cached) = tool_cache.get(&cache_key) {
                            outcomes[i] = Some(cached.clone());
                        } else {
                            let tools = tools.clone();
                            let name = name.clone();
                            let input = input.clone();
                            pending.push(async move {
                                let out = tools.dispatch(&name, input).await;
                                (i, cache_key, out)
                            });
                        }
                    }
                    for (i, cache_key, out) in futures::future::join_all(pending).await {
                        tool_cache.insert(cache_key, out.clone());
                        outcomes[i] = Some(out);
                    }
                }
                let mut results: Vec<ContentBlock> = Vec::with_capacity(tool_calls.len());
                for (i, (id, name, input)) in tool_calls.into_iter().enumerate() {
                    let outcome = if let Some(done) = outcomes[i].take() {
                        done
                    } else {
                        let cache_key =
                            (name.clone(), serde_json::to_string(&input).unwrap_or_default());
                        if let Some(cached) = tool_cache.get(&cache_key) {
                            // Cache hit: reuse the previous result without re-dispatching.
                            cached.clone()
                        } else {
                            let fresh = tools.dispatch(&name, input).await;
                            tool_cache.insert(cache_key, fresh.clone());
                            fresh
                        }
                    };
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

    fn agent_with_gate(
        responses: Vec<Vec<StreamEvent>>,
        gate: Arc<CountingGate>,
    ) -> AgentLoop {
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
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::Verification { passed: true, .. })
        ));
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
                && m.content.iter().any(|b| matches!(
                    b,
                    ContentBlock::Text { text } if text.contains("[arccode verify]")
                ))
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

    /// Dispatcher that sleeps per call and records the maximum number of
    /// calls in flight at once.
    struct ConcurrencyProbe {
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
    }

    #[async_trait]
    impl ToolDispatcher for ConcurrencyProbe {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }
        async fn dispatch(&self, _name: &str, _args: serde_json::Value) -> ToolOutcome {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            ToolOutcome::ok("ok")
        }
    }

    fn multi_tool_response(names: &[&str]) -> Vec<StreamEvent> {
        let mut events: Vec<StreamEvent> = names
            .iter()
            .enumerate()
            .map(|(i, name)| StreamEvent::ToolUse {
                block: ContentBlock::ToolUse {
                    id: format!("t{i}"),
                    name: (*name).into(),
                    input: serde_json::json!({ "n": i }),
                },
            })
            .collect();
        events.push(StreamEvent::Stop {
            reason: StopReason::ToolUse,
        });
        events
    }

    #[tokio::test]
    async fn readonly_tool_batch_runs_concurrently() {
        let probe = Arc::new(ConcurrencyProbe {
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        });
        let mut agent = AgentLoop::new(
            Arc::new(ScriptedProvider::new(vec![
                multi_tool_response(&["read_file", "grep", "glob"]),
                end_turn_response("done"),
            ])),
            probe.clone(),
            AgentConfig {
                model: "scripted/test".into(),
                ..Default::default()
            },
        );
        let events = collect_events(&mut agent).await;

        assert_eq!(probe.max_in_flight.load(Ordering::SeqCst), 3);
        // Results still arrive in call order.
        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["t0", "t1", "t2"]);
    }

    #[tokio::test]
    async fn batch_with_mutating_tool_stays_sequential() {
        let probe = Arc::new(ConcurrencyProbe {
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        });
        let mut agent = AgentLoop::new(
            Arc::new(ScriptedProvider::new(vec![
                multi_tool_response(&["read_file", "write_file", "grep"]),
                end_turn_response("done"),
            ])),
            probe.clone(),
            AgentConfig {
                model: "scripted/test".into(),
                gate: None,
                ..Default::default()
            },
        );
        let _ = collect_events(&mut agent).await;

        assert_eq!(probe.max_in_flight.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn budget_stops_session_once_exhausted() {
        // claude-opus-4-7 output is $75/MTok; 1M output tokens ≈ $75.
        let expensive_response = vec![
            StreamEvent::TextDelta {
                text: "pricey".into(),
            },
            StreamEvent::Usage {
                usage: Usage {
                    output_tokens: 1_000_000,
                    ..Default::default()
                },
            },
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ];
        let mut agent = AgentLoop::new(
            Arc::new(ScriptedProvider::new(vec![expensive_response])),
            Arc::new(OkDispatcher),
            AgentConfig {
                model: "anthropic/claude-opus-4-7".into(),
                budget_usd: Some(1.0),
                ..Default::default()
            },
        );

        // First user turn completes (spend accrues after the response)…
        let events = collect_events(&mut agent).await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Stop {
                reason: AgentStop::EndTurn
            })
        ));
        assert!(agent.spent_usd() > 70.0);

        // …the next user turn is refused without a provider call (the
        // scripted provider would panic if called again).
        let events = collect_events(&mut agent).await;
        assert!(matches!(
            events.first(),
            Some(AgentEvent::Error { message }) if message.contains("budget exhausted")
        ));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Stop {
                reason: AgentStop::Budget
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
