//! Anthropic Messages API provider.
//!
//! - POST `https://api.anthropic.com/v1/messages` with `stream=true`.
//! - SSE events parsed via `eventsource-stream`.
//! - Tool use: `input_json_delta` chunks are accumulated per-content-block,
//!   then assembled into a single `StreamEvent::ToolUse` on `content_block_stop`.
//! - Caching: `cache_control: { type: "ephemeral" }` is placed on the system
//!   prompt and on the last tool definition when the corresponding
//!   [`CacheBreakpoint`] is present.

use std::time::Duration;

use arccode_core::{
    ArccodeError, CacheBreakpoint, CacheKind, CompletionRequest, ContentBlock, Message, Provider,
    ProviderCapabilities, ProviderEventStream, Result, Role, StopReason, StreamEvent, ToolSpec,
    Usage,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    api_key: String,
    base_url: String,
    http: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| ArccodeError::Provider(format!("http client: {e}")))?;
        Ok(Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.into(),
            http,
        })
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tools: true,
            vision: true,
            cache_kind: CacheKind::Explicit,
        }
    }

    async fn complete(&self, req: CompletionRequest) -> Result<ProviderEventStream> {
        let body = build_request_body(&req);
        tracing::debug!(target: "arccode::anthropic", "request: {body}");

        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let response = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| ArccodeError::Provider(format!("anthropic request: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(ArccodeError::Provider(format!(
                "anthropic returned {status}: {text}"
            )));
        }

        let bytes = response.bytes_stream();
        let mut events = bytes.eventsource();

        // Per-content-block accumulator state.
        #[derive(Default)]
        struct BlockState {
            kind: BlockKind,
            text: String,
            partial_input: String,
            tool_id: String,
            tool_name: String,
        }
        #[derive(Default)]
        enum BlockKind {
            #[default]
            Unknown,
            Text,
            ToolUse,
        }
        let mut blocks: std::collections::HashMap<u32, BlockState> =
            std::collections::HashMap::new();

        let stream = async_stream::try_stream! {
            while let Some(item) = events.next().await {
                let evt = match item {
                    Ok(e) => e,
                    Err(e) => Err(ArccodeError::Provider(format!("sse error: {e}")))?,
                };
                if evt.event.is_empty() && evt.data.is_empty() {
                    continue;
                }
                let data: Value = match serde_json::from_str(&evt.data) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(target: "arccode::anthropic", "bad sse json: {e}: {}", evt.data);
                        continue;
                    }
                };
                match evt.event.as_str() {
                    "message_start" => {
                        if let Some(usage_val) = data.get("message").and_then(|m| m.get("usage")) {
                            if let Some(u) = parse_usage(usage_val) {
                                yield StreamEvent::Usage { usage: u };
                            }
                        }
                    }
                    "content_block_start" => {
                        let idx = data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        let cb = data.get("content_block").cloned().unwrap_or(Value::Null);
                        let kind = cb.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        let mut state = BlockState::default();
                        match kind {
                            "text" => state.kind = BlockKind::Text,
                            "tool_use" => {
                                state.kind = BlockKind::ToolUse;
                                state.tool_id = cb.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                state.tool_name = cb.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            }
                            _ => {}
                        }
                        blocks.insert(idx, state);
                    }
                    "content_block_delta" => {
                        let idx = data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        let delta = data.get("delta").cloned().unwrap_or(Value::Null);
                        let dtype = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(state) = blocks.get_mut(&idx) {
                            match dtype {
                                "text_delta" => {
                                    if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                                        state.text.push_str(t);
                                        yield StreamEvent::TextDelta { text: t.to_string() };
                                    }
                                }
                                "input_json_delta" => {
                                    if let Some(t) = delta.get("partial_json").and_then(|v| v.as_str()) {
                                        state.partial_input.push_str(t);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "content_block_stop" => {
                        let idx = data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        if let Some(state) = blocks.remove(&idx) {
                            if matches!(state.kind, BlockKind::ToolUse) {
                                let input: Value = if state.partial_input.is_empty() {
                                    Value::Object(Default::default())
                                } else {
                                    serde_json::from_str(&state.partial_input).unwrap_or_else(|_| Value::Object(Default::default()))
                                };
                                yield StreamEvent::ToolUse {
                                    block: ContentBlock::ToolUse {
                                        id: state.tool_id,
                                        name: state.tool_name,
                                        input,
                                    },
                                };
                            }
                        }
                    }
                    "message_delta" => {
                        if let Some(usage_val) = data.get("usage") {
                            if let Some(u) = parse_usage(usage_val) {
                                yield StreamEvent::Usage { usage: u };
                            }
                        }
                        let stop = data
                            .get("delta")
                            .and_then(|d| d.get("stop_reason"))
                            .and_then(|v| v.as_str())
                            .map(map_stop_reason)
                            .unwrap_or(StopReason::EndTurn);
                        yield StreamEvent::Stop { reason: stop };
                    }
                    "message_stop" => {
                        // SSE stream ends here. Stop already emitted in message_delta.
                        break;
                    }
                    "error" => {
                        let msg = data.get("error").and_then(|e| e.get("message")).and_then(|v| v.as_str()).unwrap_or("anthropic error").to_string();
                        Err(ArccodeError::Provider(msg))?;
                    }
                    _ => {
                        // ping, etc. ignore.
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

fn map_stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        _ => StopReason::Other,
    }
}

fn parse_usage(v: &Value) -> Option<Usage> {
    let field = |name: &str| v.get(name).and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    Some(Usage {
        input_tokens: field("input_tokens"),
        output_tokens: field("output_tokens"),
        cache_creation_input_tokens: field("cache_creation_input_tokens"),
        cache_read_input_tokens: field("cache_read_input_tokens"),
    })
}

fn build_request_body(req: &CompletionRequest) -> Value {
    let cache_system = req
        .cache_breakpoints
        .iter()
        .any(|b| matches!(b, CacheBreakpoint::AfterSystem));
    let cache_tools = req
        .cache_breakpoints
        .iter()
        .any(|b| matches!(b, CacheBreakpoint::AfterTools));

    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "stream": true,
    });

    if let Some(temp) = req.temperature {
        body["temperature"] = json!(temp);
    }

    if let Some(system) = &req.system {
        if cache_system {
            body["system"] = json!([{
                "type": "text",
                "text": system,
                "cache_control": { "type": "ephemeral" }
            }]);
        } else {
            body["system"] = json!(system);
        }
    }

    if !req.tools.is_empty() {
        body["tools"] = encode_tools(&req.tools, cache_tools);
    }

    // Rolling conversation cache: the caller places an `AfterMessage(n)`
    // breakpoint (typically the last message) so the growing prefix is
    // cached and later turns read it instead of re-billing every prior turn.
    // We honor the deepest requested index, clamped to the message list.
    let cache_through = req
        .cache_breakpoints
        .iter()
        .filter_map(|b| match b {
            CacheBreakpoint::AfterMessage(n) => Some(*n),
            _ => None,
        })
        .max()
        .map(|n| n.min(req.messages.len().saturating_sub(1)));

    body["messages"] = encode_messages(&req.messages, cache_through);

    body
}

fn encode_tools(tools: &[ToolSpec], cache_last: bool) -> Value {
    let last_idx = tools.len().saturating_sub(1);
    let arr: Vec<Value> = tools
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut obj = json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            });
            if cache_last && i == last_idx {
                obj["cache_control"] = json!({ "type": "ephemeral" });
            }
            obj
        })
        .collect();
    Value::Array(arr)
}

fn encode_messages(messages: &[Message], cache_through: Option<usize>) -> Value {
    let arr: Vec<Value> = messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let mut content: Vec<Value> = m.content.iter().map(encode_block).collect();
            // Mark the last content block of the cache-through message so the
            // provider caches the whole prefix up to and including it.
            if cache_through == Some(i) {
                if let Some(last) = content.last_mut() {
                    last["cache_control"] = json!({ "type": "ephemeral" });
                }
            }
            json!({ "role": role, "content": content })
        })
        .collect();
    Value::Array(arr)
}

fn encode_block(b: &ContentBlock) -> Value {
    match b {
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::ToolUse { id, name, input } => json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error,
        }),
        ContentBlock::Image { data, media_type } => json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            },
        }),
    }
}

/// Anthropic API request/response stubs kept for type-level documentation;
/// we serialize through `serde_json::Value` for flexibility.
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
struct ApiError {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use arccode_core::ContentBlock;

    #[test]
    fn request_body_includes_cache_control_on_system() {
        let mut req = CompletionRequest::new("claude-opus-4-7");
        req.system = Some("you are a helpful agent".into());
        req.cache_breakpoints = vec![CacheBreakpoint::AfterSystem];
        let body = build_request_body(&req);
        let system = &body["system"];
        assert!(system.is_array(), "system should be array when caching");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn after_message_caches_last_block_of_that_message() {
        use arccode_core::{Message, Role};
        let mut req = CompletionRequest::new("claude-opus-4-8");
        req.messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "a".into() }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "b".into() }],
            },
        ];
        req.cache_breakpoints = vec![CacheBreakpoint::AfterMessage(1)];
        let body = build_request_body(&req);
        let msgs = body["messages"].as_array().unwrap();
        assert!(
            msgs[0]["content"][0].get("cache_control").is_none(),
            "earlier message must not be marked"
        );
        assert_eq!(
            msgs[1]["content"][0]["cache_control"]["type"], "ephemeral",
            "cache-through message's last block must carry cache_control"
        );
    }

    #[test]
    fn after_message_index_clamps_to_last_message() {
        use arccode_core::{Message, Role};
        let mut req = CompletionRequest::new("claude-opus-4-8");
        req.messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "a".into() }],
        }];
        // An out-of-range index (stale rolling breakpoint) clamps to the last
        // message rather than dropping the breakpoint.
        req.cache_breakpoints = vec![CacheBreakpoint::AfterMessage(99)];
        let body = build_request_body(&req);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn request_body_uses_string_system_without_cache() {
        let mut req = CompletionRequest::new("claude-opus-4-7");
        req.system = Some("hi".into());
        let body = build_request_body(&req);
        assert_eq!(body["system"], json!("hi"));
    }

    #[test]
    fn tool_result_block_serializes_with_is_error_flag() {
        let b = ContentBlock::ToolResult {
            tool_use_id: "abc".into(),
            content: "oops".into(),
            is_error: true,
        };
        let v = encode_block(&b);
        assert_eq!(v["type"], "tool_result");
        assert_eq!(v["is_error"], true);
        assert_eq!(v["content"], "oops");
    }

    #[test]
    fn cache_control_on_last_tool_only() {
        let tools = vec![
            ToolSpec {
                name: "a".into(),
                description: "".into(),
                input_schema: json!({}),
            },
            ToolSpec {
                name: "b".into(),
                description: "".into(),
                input_schema: json!({}),
            },
        ];
        let v = encode_tools(&tools, true);
        let arr = v.as_array().unwrap();
        assert!(arr[0].get("cache_control").is_none());
        assert_eq!(arr[1]["cache_control"]["type"], "ephemeral");
    }
}
