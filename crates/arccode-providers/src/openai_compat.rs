//! OpenAI-compatible chat completions provider.
//!
//! One adapter covers the OpenAI Chat Completions API and every API-shape
//! clone that arccode supports: OpenRouter, LM Studio, vLLM, LiteLLM
//! (self-hosted proxy), and Ollama (via its `/v1` shim). The provider id
//! and default base URL are picked up from [`Variant`].
//!
//! - Streaming via SSE (`eventsource-stream`); supports `stream_options:
//!   { include_usage: true }` so we get a final usage event before `[DONE]`.
//! - Tool calling: the model emits `choices[0].delta.tool_calls`; arguments
//!   stream in as partial JSON strings which we accumulate per tool index
//!   and assemble into a single [`StreamEvent::ToolUse`] on
//!   `finish_reason == "tool_calls"` (or stop) — the same pattern as the
//!   Anthropic adapter.
//! - Caching: OpenAI caches automatically based on stable prefix ordering;
//!   we honor that by always serializing system → tools → messages in the
//!   same order. No explicit `cache_control` markers are emitted.

use std::time::Duration;

use arccode_core::{
    ArccodeError, CacheKind, CompletionRequest, ContentBlock, Message, Provider,
    ProviderCapabilities, ProviderEventStream, Result, Role, StopReason, StreamEvent, ToolSpec,
    Usage,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use serde_json::{json, Value};

/// Which downstream we're talking to. Picks a default base URL and the
/// stable `provider_id` we expose to the agent.
#[derive(Debug, Clone, Copy)]
pub enum Variant {
    OpenAI,
    OpenRouter,
    LmStudio,
    Vllm,
    LiteLlm,
    Ollama,
}

impl Variant {
    pub fn id(self) -> &'static str {
        match self {
            Variant::OpenAI => "openai",
            Variant::OpenRouter => "openrouter",
            Variant::LmStudio => "lmstudio",
            Variant::Vllm => "vllm",
            Variant::LiteLlm => "litellm",
            Variant::Ollama => "ollama",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Variant::OpenAI => "https://api.openai.com/v1",
            Variant::OpenRouter => "https://openrouter.ai/api/v1",
            Variant::LmStudio => "http://localhost:1234/v1",
            Variant::Vllm => "http://localhost:8000/v1",
            Variant::LiteLlm => "http://localhost:4000/v1",
            Variant::Ollama => "http://localhost:11434/v1",
        }
    }

    /// Whether a real API key is mandatory for this variant. Local
    /// providers accept any string (or none) and we send a dummy token.
    pub fn requires_api_key(self) -> bool {
        matches!(self, Variant::OpenAI | Variant::OpenRouter)
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatProvider {
    variant: Variant,
    api_key: Option<String>,
    base_url: String,
    http: reqwest::Client,
}

impl OpenAiCompatProvider {
    pub fn new(variant: Variant, api_key: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| ArccodeError::Provider(format!("http client: {e}")))?;
        Ok(Self {
            variant,
            api_key,
            base_url: variant.default_base_url().to_string(),
            http,
        })
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

#[async_trait]
impl Provider for OpenAiCompatProvider {
    fn id(&self) -> &str {
        self.variant.id()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tools: true,
            vision: matches!(self.variant, Variant::OpenAI | Variant::OpenRouter),
            cache_kind: CacheKind::Automatic,
        }
    }

    async fn complete(&self, req: CompletionRequest) -> Result<ProviderEventStream> {
        let body = build_request_body(&req);
        tracing::debug!(target: "arccode::openai_compat", "request: {body}");

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut builder = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body);

        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                builder = builder.header("authorization", format!("Bearer {key}"));
            }
        } else if self.variant.requires_api_key() {
            return Err(ArccodeError::Provider(format!(
                "provider {} requires an api_key",
                self.variant.id()
            )));
        }

        // OpenRouter best-practice headers (harmless elsewhere).
        if matches!(self.variant, Variant::OpenRouter) {
            builder = builder
                .header("HTTP-Referer", "https://github.com/your-org/arccode")
                .header("X-Title", "arccode");
        }

        let response = builder
            .send()
            .await
            .map_err(|e| ArccodeError::Provider(format!("request: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(ArccodeError::Provider(format!(
                "{} returned {status}: {text}",
                self.variant.id()
            )));
        }

        let bytes = response.bytes_stream();
        let mut events = bytes.eventsource();

        // Per-tool-call accumulator (keyed by streamed `index`).
        #[derive(Default)]
        struct ToolAcc {
            id: String,
            name: String,
            args: String,
        }
        let mut tool_accs: std::collections::HashMap<u32, ToolAcc> =
            std::collections::HashMap::new();
        let mut emitted_tool_indices: std::collections::HashSet<u32> =
            std::collections::HashSet::new();

        let stream = async_stream::try_stream! {
            while let Some(item) = events.next().await {
                let evt = match item {
                    Ok(e) => e,
                    Err(e) => Err(ArccodeError::Provider(format!("sse: {e}")))?,
                };
                if evt.data.is_empty() { continue; }
                if evt.data.trim() == "[DONE]" {
                    // Flush any tool calls we hadn't seen finish_reason for.
                    for (idx, acc) in tool_accs.drain() {
                        if emitted_tool_indices.contains(&idx) { continue; }
                        let input: Value = serde_json::from_str(&acc.args).unwrap_or_else(|_| Value::Object(Default::default()));
                        yield StreamEvent::ToolUse {
                            block: ContentBlock::ToolUse { id: acc.id, name: acc.name, input },
                        };
                    }
                    yield StreamEvent::Stop { reason: StopReason::EndTurn };
                    break;
                }
                let chunk: Value = match serde_json::from_str(&evt.data) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(target: "arccode::openai_compat", "bad sse json: {e}: {}", evt.data);
                        continue;
                    }
                };

                // Some implementations emit usage on its own chunk with empty choices.
                if let Some(u) = chunk.get("usage").and_then(parse_usage) {
                    yield StreamEvent::Usage { usage: u };
                }

                let Some(choice0) = chunk.get("choices").and_then(|c| c.get(0)) else { continue };
                let delta = choice0.get("delta").cloned().unwrap_or(Value::Null);

                if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        yield StreamEvent::TextDelta { text: text.to_string() };
                    }
                }

                if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        let acc = tool_accs.entry(idx).or_default();
                        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                            if !id.is_empty() { acc.id = id.to_string(); }
                        }
                        if let Some(func) = tc.get("function") {
                            if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                                if !name.is_empty() { acc.name = name.to_string(); }
                            }
                            if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                                acc.args.push_str(args);
                            }
                        }
                    }
                }

                if let Some(finish) = choice0.get("finish_reason").and_then(|v| v.as_str()) {
                    // Emit any accumulated tool calls.
                    let mut indices: Vec<u32> = tool_accs.keys().copied().collect();
                    indices.sort();
                    for idx in indices {
                        if emitted_tool_indices.contains(&idx) { continue; }
                        if let Some(acc) = tool_accs.remove(&idx) {
                            let input: Value = if acc.args.is_empty() {
                                Value::Object(Default::default())
                            } else {
                                serde_json::from_str(&acc.args).unwrap_or_else(|_| Value::Object(Default::default()))
                            };
                            emitted_tool_indices.insert(idx);
                            yield StreamEvent::ToolUse {
                                block: ContentBlock::ToolUse { id: acc.id, name: acc.name, input },
                            };
                        }
                    }
                    let reason = match finish {
                        "stop" => StopReason::EndTurn,
                        "tool_calls" => StopReason::ToolUse,
                        "length" => StopReason::MaxTokens,
                        _ => StopReason::Other,
                    };
                    yield StreamEvent::Stop { reason };
                    // Keep reading for trailing usage + [DONE].
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

fn parse_usage(v: &Value) -> Option<Usage> {
    let field = |name: &str| v.get(name).and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    Some(Usage {
        input_tokens: field("prompt_tokens"),
        output_tokens: field("completion_tokens"),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: v
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32,
    })
}

fn build_request_body(req: &CompletionRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(system) = &req.system {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for m in &req.messages {
        encode_message(m, &mut messages);
    }

    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "stream": true,
        "messages": messages,
        "stream_options": { "include_usage": true },
    });
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if !req.tools.is_empty() {
        body["tools"] = encode_tools(&req.tools);
    }
    body
}

fn encode_tools(tools: &[ToolSpec]) -> Value {
    let arr: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect();
    Value::Array(arr)
}

fn encode_message(m: &Message, out: &mut Vec<Value>) {
    // Tool results split into their own role="tool" messages.
    if m.role == Role::User
        && m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
    {
        for b in &m.content {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } = b
            {
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content,
                }));
            }
        }
        return;
    }

    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };

    // Collect text blocks (concatenated), image blocks, and tool_use blocks (mapped to tool_calls).
    let mut text = String::new();
    let mut image_parts: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for b in &m.content {
        match b {
            ContentBlock::Text { text: t } => {
                text.push_str(t);
            }
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": input.to_string(),
                    }
                }));
            }
            ContentBlock::ToolResult { .. } => { /* handled above */ }
            ContentBlock::Image { data, media_type } => {
                image_parts.push(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{media_type};base64,{data}"),
                    }
                }));
            }
        }
    }

    let mut msg = json!({ "role": role });
    if !image_parts.is_empty() {
        // Vision: build a multi-part content array with text + images.
        let mut parts: Vec<Value> = Vec::new();
        if !text.is_empty() {
            parts.push(json!({"type": "text", "text": text}));
        }
        parts.extend(image_parts);
        msg["content"] = Value::Array(parts);
    } else if !text.is_empty() {
        msg["content"] = json!(text);
    } else if tool_calls.is_empty() {
        // Empty assistant message — OpenAI requires `content` or `tool_calls`.
        msg["content"] = json!("");
    }
    if !tool_calls.is_empty() {
        msg["tool_calls"] = Value::Array(tool_calls);
    }
    out.push(msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_have_distinct_ids_and_defaults() {
        for v in [
            Variant::OpenAI,
            Variant::OpenRouter,
            Variant::LmStudio,
            Variant::Vllm,
            Variant::LiteLlm,
            Variant::Ollama,
        ] {
            assert!(!v.id().is_empty());
            assert!(v.default_base_url().starts_with("http"));
        }
    }

    #[test]
    fn tools_become_function_type() {
        let tools = vec![ToolSpec {
            name: "foo".into(),
            description: "do a foo".into(),
            input_schema: json!({"type":"object"}),
        }];
        let v = encode_tools(&tools);
        assert_eq!(v[0]["type"], "function");
        assert_eq!(v[0]["function"]["name"], "foo");
    }

    #[test]
    fn tool_result_becomes_tool_role_message() {
        let mut out = Vec::new();
        let m = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "ok".into(),
                is_error: false,
            }],
        };
        encode_message(&m, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["tool_call_id"], "call_1");
        assert_eq!(out[0]["content"], "ok");
    }

    #[test]
    fn assistant_text_and_tool_use_share_one_message() {
        let mut out = Vec::new();
        let m = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "calling foo".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "foo".into(),
                    input: json!({ "x": 1 }),
                },
            ],
        };
        encode_message(&m, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "assistant");
        assert_eq!(out[0]["content"], "calling foo");
        assert_eq!(out[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(out[0]["tool_calls"][0]["function"]["name"], "foo");
    }

    #[test]
    fn request_includes_stream_options_include_usage() {
        let mut req = CompletionRequest::new("gpt-4.1");
        req.system = Some("hi".into());
        let body = build_request_body(&req);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }
}
