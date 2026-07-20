//! Cohere v2 chat completions provider.
//!
//! Endpoint: `POST /v2/chat` against `https://api.cohere.com` with
//! `Authorization: Bearer <COHERE_API_KEY>`.
//!
//! Cohere's v2 chat shape is close to OpenAI's (role + content messages,
//! `tools` declared as JSON-schema functions) but the **streaming event
//! envelope is different**: each SSE chunk has a top-level `type` field
//! (`message-start`, `content-delta`, `tool-call-start`,
//! `tool-call-delta`, `tool-call-end`, `message-end`) with a nested
//! `delta.message` payload. That's why this lives in its own file
//! instead of riding on [`crate::OpenAiCompatProvider`].
//!
//! Caching: Cohere offers an automatic system-prompt cache. We don't emit
//! explicit cache markers — the prompt prefix is already stable because
//! we always serialize system → tools → messages in order.

use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use serde_json::{json, Value};
use wingman_core::{
    CacheKind, CompletionRequest, ContentBlock, Message, Provider, ProviderCapabilities,
    ProviderEventStream, Result, Role, StopReason, StreamEvent, ToolSpec, Usage, WingmanError,
};

const DEFAULT_BASE_URL: &str = "https://api.cohere.com";

#[derive(Debug, Clone)]
pub struct CohereProvider {
    api_key: String,
    base_url: String,
    http: reqwest::Client,
}

impl CohereProvider {
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| WingmanError::Provider(format!("http client: {e}")))?;
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
impl Provider for CohereProvider {
    fn id(&self) -> &str {
        "cohere"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tools: true,
            // Command-A and Command-R-Plus 08-2024 support image inputs in
            // the same `image_url` shape OpenAI uses.
            vision: true,
            cache_kind: CacheKind::Automatic,
        }
    }

    async fn complete(&self, req: CompletionRequest) -> Result<ProviderEventStream> {
        let body = build_request_body(&req);
        tracing::debug!(target: "wingman::cohere", "request: {body}");

        let url = format!("{}/v2/chat", self.base_url.trim_end_matches('/'));
        let response = crate::retry::send_with_retry("cohere", || {
            self.http
                .post(&url)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .header("authorization", format!("Bearer {}", self.api_key))
                .json(&body)
                .send()
        })
        .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(WingmanError::Provider(format!(
                "cohere returned {status}: {text}"
            )));
        }

        let bytes = response.bytes_stream();
        let mut events = bytes.eventsource();

        // Per-tool-call accumulator keyed by streamed `index`.
        #[derive(Default)]
        struct ToolAcc {
            id: String,
            name: String,
            args: String,
        }
        let mut tool_accs: std::collections::HashMap<u32, ToolAcc> =
            std::collections::HashMap::new();

        let stream = async_stream::try_stream! {
            while let Some(item) = events.next().await {
                let evt = match item {
                    Ok(e) => e,
                    Err(e) => Err(WingmanError::Provider(format!("sse: {e}")))?,
                };
                if evt.data.is_empty() { continue; }
                if evt.data.trim() == "[DONE]" {
                    yield StreamEvent::Stop { reason: StopReason::EndTurn };
                    break;
                }
                let chunk: Value = match serde_json::from_str(&evt.data) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(target: "wingman::cohere", "bad sse json: {e}: {}", evt.data);
                        continue;
                    }
                };

                let ty = chunk.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let delta = chunk.pointer("/delta/message").cloned().unwrap_or(Value::Null);

                match ty {
                    "content-delta" => {
                        if let Some(text) = delta
                            .pointer("/content/text")
                            .and_then(|v| v.as_str())
                        {
                            if !text.is_empty() {
                                yield StreamEvent::TextDelta { text: text.to_string() };
                            }
                        }
                    }
                    "tool-call-start" => {
                        let idx = chunk
                            .get("index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32;
                        let acc = tool_accs.entry(idx).or_default();
                        if let Some(id) = delta
                            .pointer("/tool_calls/id")
                            .and_then(|v| v.as_str())
                        {
                            acc.id = id.to_string();
                        }
                        if let Some(name) = delta
                            .pointer("/tool_calls/function/name")
                            .and_then(|v| v.as_str())
                        {
                            acc.name = name.to_string();
                        }
                        if let Some(args) = delta
                            .pointer("/tool_calls/function/arguments")
                            .and_then(|v| v.as_str())
                        {
                            acc.args.push_str(args);
                        }
                    }
                    "tool-call-delta" => {
                        let idx = chunk
                            .get("index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32;
                        let acc = tool_accs.entry(idx).or_default();
                        if let Some(args) = delta
                            .pointer("/tool_calls/function/arguments")
                            .and_then(|v| v.as_str())
                        {
                            acc.args.push_str(args);
                        }
                    }
                    "tool-call-end" => {
                        let idx = chunk
                            .get("index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32;
                        if let Some(acc) = tool_accs.remove(&idx) {
                            let input: Value = if acc.args.is_empty() {
                                Value::Object(Default::default())
                            } else {
                                serde_json::from_str(&acc.args)
                                    .unwrap_or_else(|_| Value::Object(Default::default()))
                            };
                            yield StreamEvent::ToolUse {
                                block: ContentBlock::ToolUse {
                                    id: acc.id,
                                    name: acc.name,
                                    input,
                                },
                            };
                        }
                    }
                    "message-end" => {
                        // Flush any tool calls that didn't get a tool-call-end.
                        let mut indices: Vec<u32> = tool_accs.keys().copied().collect();
                        indices.sort();
                        for idx in indices {
                            if let Some(acc) = tool_accs.remove(&idx) {
                                let input: Value = serde_json::from_str(&acc.args)
                                    .unwrap_or_else(|_| Value::Object(Default::default()));
                                yield StreamEvent::ToolUse {
                                    block: ContentBlock::ToolUse {
                                        id: acc.id,
                                        name: acc.name,
                                        input,
                                    },
                                };
                            }
                        }
                        // Usage is reported in delta.usage.billed_units.
                        if let Some(u) = chunk
                            .pointer("/delta/usage/billed_units")
                            .and_then(parse_usage)
                        {
                            yield StreamEvent::Usage { usage: u };
                        }
                        let reason = match chunk
                            .pointer("/delta/finish_reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("COMPLETE")
                        {
                            "COMPLETE" => StopReason::EndTurn,
                            "TOOL_CALL" => StopReason::ToolUse,
                            "MAX_TOKENS" => StopReason::MaxTokens,
                            _ => StopReason::Other,
                        };
                        yield StreamEvent::Stop { reason };
                    }
                    _ => {} // message-start, content-start, content-end — ignore.
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

fn parse_usage(v: &Value) -> Option<Usage> {
    let field = |name: &str| v.get(name).and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    Some(Usage {
        input_tokens: field("input_tokens"),
        output_tokens: field("output_tokens"),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
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
    // Tool results are their own role="tool" messages.
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

    let mut text = String::new();
    let mut images: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for b in &m.content {
        match b {
            ContentBlock::Text { text: t } => text.push_str(t),
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
            ContentBlock::ToolResult { .. } => {}
            // Command-A / Command-R-Plus accept OpenAI-shape image_url parts.
            ContentBlock::Image { data, media_type } => {
                images.push(json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{media_type};base64,{data}") },
                }));
            }
        }
    }

    let mut msg = json!({ "role": role });
    if !images.is_empty() {
        // Multi-part content: an optional text part followed by image parts.
        let mut parts: Vec<Value> = Vec::new();
        if !text.is_empty() {
            parts.push(json!({ "type": "text", "text": text }));
        }
        parts.extend(images);
        msg["content"] = Value::Array(parts);
    } else if !text.is_empty() {
        msg["content"] = json!(text);
    } else if tool_calls.is_empty() {
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
    fn request_includes_stream_and_messages() {
        let mut req = CompletionRequest::new("command-r-plus");
        req.system = Some("you are helpful".into());
        req.messages.push(Message::user_text("hi"));
        let body = build_request_body(&req);
        assert_eq!(body["stream"], true);
        assert_eq!(body["model"], "command-r-plus");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
    }

    #[test]
    fn tools_become_function_type() {
        let tools = vec![ToolSpec {
            name: "do_thing".into(),
            description: "do a thing".into(),
            input_schema: json!({"type":"object"}),
        }];
        let v = encode_tools(&tools);
        assert_eq!(v[0]["type"], "function");
        assert_eq!(v[0]["function"]["name"], "do_thing");
    }

    #[test]
    fn tool_result_becomes_tool_role_message() {
        let mut out = Vec::new();
        encode_message(
            &Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_x".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
            },
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["tool_call_id"], "call_x");
    }

    #[test]
    fn image_becomes_multipart_content_with_image_url() {
        let mut out = Vec::new();
        encode_message(
            &Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text {
                        text: "what is this".into(),
                    },
                    ContentBlock::Image {
                        data: "AAAA".into(),
                        media_type: "image/png".into(),
                    },
                ],
            },
            &mut out,
        );
        assert_eq!(out.len(), 1);
        let parts = out[0]["content"].as_array().expect("array content");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "what is this");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }
}
