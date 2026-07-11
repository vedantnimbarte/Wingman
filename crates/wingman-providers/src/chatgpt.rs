//! ChatGPT subscription provider — uses OpenAI's Codex Responses API endpoint
//! (`https://chatgpt.com/backend-api/codex/responses`) with an OAuth 2.0
//! access token obtained via the PKCE browser flow (`wingman auth login`).
//!
//! Request/response format follows the OpenAI Responses API (not Chat
//! Completions): messages are encoded in `input`, the system prompt goes in
//! `instructions`, and SSE events carry `response.output_text.delta` /
//! `response.output_item.done` / `response.completed` event types.
//!
//! Token storage: the access token lives in the OS keychain under the service
//! name `"chatgpt"`.  The companion refresh token is stored under
//! `"chatgpt_refresh"`.  Both are managed by `wingman-cli`'s runtime and OAuth
//! modules; this provider only consumes the already-resolved access token.

use std::time::Duration;

use wingman_core::{
    WingmanError, CacheKind, CompletionRequest, ContentBlock, Message, Provider,
    ProviderCapabilities, ProviderEventStream, Result, Role, StopReason, StreamEvent, ToolSpec,
    Usage,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};

const CODEX_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

#[derive(Debug, Clone)]
pub struct ChatGptProvider {
    access_token: String,
    http: reqwest::Client,
}

impl ChatGptProvider {
    pub fn new(access_token: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| WingmanError::Provider(format!("http client: {e}")))?;
        Ok(Self {
            access_token: access_token.into(),
            http,
        })
    }
}

#[async_trait]
impl Provider for ChatGptProvider {
    fn id(&self) -> &str {
        "chatgpt"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tools: true,
            vision: false,
            cache_kind: CacheKind::None,
        }
    }

    async fn complete(&self, req: CompletionRequest) -> Result<ProviderEventStream> {
        let body = build_request_body(&req);
        tracing::debug!(target: "wingman::chatgpt", "request: {body}");

        let response = crate::retry::send_with_retry("chatgpt", || {
            self.http
                .post(CODEX_URL)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .header("authorization", format!("Bearer {}", self.access_token))
                .json(&body)
                .send()
        })
        .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            if status.as_u16() == 401 {
                return Err(WingmanError::Provider(
                    "chatgpt: token expired — run /login to re-authenticate".into(),
                ));
            }
            return Err(WingmanError::Provider(format!(
                "chatgpt returned {status}: {text}"
            )));
        }

        let bytes = response.bytes_stream();
        let mut events = bytes.eventsource();

        // Accumulator for streaming function-call arguments, keyed by output_index.
        #[derive(Default)]
        struct FnAcc {
            call_id: String,
            name: String,
            args: String,
        }
        let mut fn_accs: std::collections::HashMap<u32, FnAcc> = std::collections::HashMap::new();

        let stream = async_stream::try_stream! {
            while let Some(item) = events.next().await {
                let evt = match item {
                    Ok(e) => e,
                    Err(e) => Err(WingmanError::Provider(format!("sse: {e}")))?,
                };
                if evt.data.is_empty() { continue; }
                if evt.data.trim() == "[DONE]" { break; }

                let data: Value = match serde_json::from_str(&evt.data) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            target: "wingman::chatgpt",
                            "bad sse json ({e}): {}",
                            evt.data
                        );
                        continue;
                    }
                };

                let ev_type = data.get("type").and_then(|v| v.as_str()).unwrap_or("");

                match ev_type {
                    "response.output_text.delta" => {
                        if let Some(delta) = data.get("delta").and_then(|v| v.as_str()) {
                            if !delta.is_empty() {
                                yield StreamEvent::TextDelta { text: delta.to_string() };
                            }
                        }
                    }

                    "response.function_call_arguments.delta" => {
                        let idx = data
                            .get("output_index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32;
                        let acc = fn_accs.entry(idx).or_default();
                        if let Some(delta) = data.get("delta").and_then(|v| v.as_str()) {
                            acc.args.push_str(delta);
                        }
                    }

                    "response.output_item.added" => {
                        // Capture call_id and name when the function-call item first appears.
                        if let Some(item) = data.get("item") {
                            if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                                let idx = data
                                    .get("output_index")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0) as u32;
                                let acc = fn_accs.entry(idx).or_default();
                                if let Some(id) = item.get("call_id").and_then(|v| v.as_str()) {
                                    acc.call_id = id.to_string();
                                }
                                if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                                    acc.name = name.to_string();
                                }
                            }
                        }
                    }

                    "response.output_item.done" => {
                        if let Some(item) = data.get("item") {
                            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            if item_type == "function_call" {
                                let idx = data
                                    .get("output_index")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0) as u32;
                                // Prefer accumulated streaming args; fall back to the final item.
                                let (call_id, name, args_str) = if let Some(acc) = fn_accs.remove(&idx) {
                                    (acc.call_id, acc.name, acc.args)
                                } else {
                                    let id = item
                                        .get("call_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let nm = item
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let ag = item
                                        .get("arguments")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("{}")
                                        .to_string();
                                    (id, nm, ag)
                                };
                                let input: Value = serde_json::from_str(&args_str)
                                    .unwrap_or_else(|_| Value::Object(Default::default()));
                                yield StreamEvent::ToolUse {
                                    block: ContentBlock::ToolUse {
                                        id: call_id,
                                        name,
                                        input,
                                    },
                                };
                            }
                        }
                    }

                    "response.completed" => {
                        if let Some(resp) = data.get("response") {
                            if let Some(u) = resp.get("usage").and_then(parse_usage) {
                                yield StreamEvent::Usage { usage: u };
                            }
                            // Check whether any tool calls were made.
                            let has_tools = resp
                                .get("output")
                                .and_then(|o| o.as_array())
                                .map(|arr| {
                                    arr.iter().any(|item| {
                                        item.get("type").and_then(|v| v.as_str())
                                            == Some("function_call")
                                    })
                                })
                                .unwrap_or(false);
                            let reason = if has_tools {
                                StopReason::ToolUse
                            } else {
                                StopReason::EndTurn
                            };
                            yield StreamEvent::Stop { reason };
                        } else {
                            yield StreamEvent::Stop { reason: StopReason::EndTurn };
                        }
                        break;
                    }

                    "response.failed" | "error" => {
                        let msg = data
                            .get("response")
                            .and_then(|r| r.get("error"))
                            .and_then(|e| e.get("message"))
                            .and_then(|v| v.as_str())
                            .or_else(|| data.get("message").and_then(|v| v.as_str()))
                            .unwrap_or("unknown error from chatgpt");
                        Err(WingmanError::Provider(format!("chatgpt: {msg}")))?;
                    }

                    _ => {
                        tracing::trace!(target: "wingman::chatgpt", "unhandled event type: {ev_type}");
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

fn parse_usage(v: &Value) -> Option<Usage> {
    let field = |name: &str| v.get(name).and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    // input_tokens includes cached_tokens; subtract so the cached slice isn't
    // billed at the full input rate on top of the cache-read rate. Reasoning
    // tokens are already folded into output_tokens by the Responses API.
    let cached = v
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    Some(Usage {
        input_tokens: field("input_tokens").saturating_sub(cached),
        output_tokens: field("output_tokens"),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
    })
}

fn build_request_body(req: &CompletionRequest) -> Value {
    let input = encode_input(&req.messages);

    let mut body = json!({
        "model": req.model,
        "input": input,
        "stream": true,
        "max_output_tokens": req.max_tokens,
    });

    if let Some(system) = &req.system {
        body["instructions"] = json!(system);
    }

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
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            })
        })
        .collect();
    Value::Array(arr)
}

/// Encode arc-code messages into the Responses API `input` array.
///
/// Mapping:
/// - User text  → `{"role":"user","content":[{"type":"input_text","text":...}]}`
/// - ToolResult → `{"type":"function_call_output","call_id":...,"output":...}`
/// - Assistant text → `{"role":"assistant","content":[{"type":"output_text","text":...}]}`
/// - ToolUse (assistant) → `{"type":"function_call","call_id":...,"name":...,"arguments":...}`
fn encode_input(messages: &[Message]) -> Value {
    let mut items: Vec<Value> = Vec::new();

    for m in messages {
        match m.role {
            Role::User => {
                let mut text_parts: Vec<Value> = Vec::new();
                let mut tool_results: Vec<Value> = Vec::new();

                for b in &m.content {
                    match b {
                        ContentBlock::Text { text } => {
                            text_parts.push(json!({"type": "input_text", "text": text}));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            tool_results.push(json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": content,
                            }));
                        }
                        ContentBlock::Image { data, media_type } => {
                            text_parts.push(json!({
                                "type": "input_image",
                                "image_url": format!("data:{media_type};base64,{data}"),
                            }));
                        }
                        ContentBlock::ToolUse { .. } => {}
                    }
                }

                // Tool results are top-level items, not inside a role message.
                items.extend(tool_results);

                if !text_parts.is_empty() {
                    items.push(json!({
                        "role": "user",
                        "content": text_parts,
                    }));
                }
            }

            Role::Assistant => {
                let mut text_parts: Vec<Value> = Vec::new();

                for b in &m.content {
                    match b {
                        ContentBlock::Text { text } => {
                            text_parts.push(json!({"type": "output_text", "text": text}));
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            items.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": input.to_string(),
                            }));
                        }
                        _ => {}
                    }
                }

                if !text_parts.is_empty() {
                    items.push(json!({
                        "role": "assistant",
                        "content": text_parts,
                    }));
                }
            }
        }
    }

    Value::Array(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_encodes_as_input_text() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        }];
        let v = encode_input(&msgs);
        assert_eq!(v[0]["role"], "user");
        assert_eq!(v[0]["content"][0]["type"], "input_text");
        assert_eq!(v[0]["content"][0]["text"], "hello");
    }

    #[test]
    fn tool_result_becomes_function_call_output() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "ok".into(),
                is_error: false,
            }],
        }];
        let v = encode_input(&msgs);
        assert_eq!(v[0]["type"], "function_call_output");
        assert_eq!(v[0]["call_id"], "call_1");
        assert_eq!(v[0]["output"], "ok");
    }

    #[test]
    fn tool_use_becomes_function_call() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "foo".into(),
                input: json!({"x": 1}),
            }],
        }];
        let v = encode_input(&msgs);
        assert_eq!(v[0]["type"], "function_call");
        assert_eq!(v[0]["call_id"], "call_1");
        assert_eq!(v[0]["name"], "foo");
    }

    #[test]
    fn request_body_has_instructions_and_stream() {
        let mut req = CompletionRequest::new("gpt-4o");
        req.system = Some("be helpful".into());
        let body = build_request_body(&req);
        assert_eq!(body["instructions"], "be helpful");
        assert_eq!(body["stream"], true);
    }
}
