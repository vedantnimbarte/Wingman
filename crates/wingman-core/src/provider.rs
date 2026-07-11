use crate::{Message, ProviderEventStream, Result, ToolSpec};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A single completion request. Includes provider-agnostic cache breakpoints —
/// each `Provider` impl decides how to honor them (Anthropic: `cache_control`
/// blocks; OpenAI: stable prefix ordering; Gemini: `cachedContent` resources).
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub cache_breakpoints: Vec<CacheBreakpoint>,
}

impl CompletionRequest {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 4096,
            temperature: None,
            cache_breakpoints: Vec::new(),
        }
    }
}

/// Where to place a cache breakpoint. Providers without explicit cache
/// control ignore these; providers with explicit control insert markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheBreakpoint {
    /// Cache the system prompt (and tools if `AfterTools` is also set).
    AfterSystem,
    /// Cache through the tool definitions.
    AfterTools,
    /// Cache through message index `n` (inclusive).
    AfterMessage(usize),
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderCapabilities {
    pub streaming: bool,
    pub tools: bool,
    pub vision: bool,
    pub cache_kind: CacheKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    /// Provider does not expose any prompt-cache mechanism.
    None,
    /// Caller marks cache breakpoints explicitly (Anthropic).
    Explicit,
    /// Provider caches automatically based on shared prefixes (OpenAI).
    Automatic,
    /// Cached content is a first-class resource the caller creates and
    /// references by id (Gemini `cachedContent`).
    Cached,
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable id like "anthropic", "openai", "gemini", "ollama", "openrouter".
    fn id(&self) -> &str;

    fn capabilities(&self) -> ProviderCapabilities;

    /// Issue a request and return a stream of `StreamEvent`s. The stream
    /// ends after a single `Stop` event.
    async fn complete(&self, req: CompletionRequest) -> Result<ProviderEventStream>;

    /// List the model ids the provider currently advertises (e.g. via an
    /// OpenAI-compatible `GET /models` endpoint). Used by the `/model`
    /// picker to show a live catalog instead of a hardcoded one.
    ///
    /// The default returns an error so providers without a listing endpoint
    /// fall back to the static catalog; providers that can enumerate models
    /// override this.
    async fn list_models(&self) -> Result<Vec<String>> {
        Err(crate::WingmanError::Provider(
            "this provider does not support listing models".into(),
        ))
    }
}

/// One-shot, tool-free text completion: issue `req` and concatenate every
/// `TextDelta` until the stream stops. For simple side calls (session
/// distillation, title/commit-message generation) that don't need the agent
/// loop. Ignores tool-use events (send a request with no tools).
pub async fn complete_text(provider: &dyn Provider, req: CompletionRequest) -> Result<String> {
    use futures::StreamExt;
    let mut stream = provider.complete(req).await?;
    let mut out = String::new();
    while let Some(ev) = stream.next().await {
        match ev? {
            crate::StreamEvent::TextDelta { text } => out.push_str(&text),
            crate::StreamEvent::Stop { .. } => break,
            _ => {}
        }
    }
    Ok(out)
}
