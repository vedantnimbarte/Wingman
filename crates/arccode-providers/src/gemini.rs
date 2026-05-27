//! Google Gemini provider via the native `generateContent` API.
//!
//! - Endpoint: `https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?alt=sse`.
//! - Streaming: SSE chunks each contain a partial response with
//!   `candidates[0].content.parts` to merge.
//! - Tool calling: tools are declared under `tools[0].functionDeclarations`;
//!   the model emits `parts[].functionCall { name, args }` blocks.
//!   Results come back via `parts[].functionResponse { name, response }`.
//! - Caching (cachedContent) is **not** wired in M2 — it requires a side
//!   call to create the cache resource and reference it by id; we'll plumb
//!   it through [`crate::tokens::CacheStrategy`] in a follow-up.

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

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
const API_VERSION: &str = "v1beta";

#[derive(Debug, Clone)]
pub struct GeminiProvider {
    api_key: String,
    base_url: String,
    http: reqwest::Client,
}

impl GeminiProvider {
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
impl Provider for GeminiProvider {
    fn id(&self) -> &str {
        "gemini"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tools: true,
            vision: true,
            cache_kind: CacheKind::Cached,
        }
    }

    async fn complete(&self, req: CompletionRequest) -> Result<ProviderEventStream> {
        let body = build_request_body(&req);
        tracing::debug!(target: "arccode::gemini", "request: {body}");

        let url = format!(
            "{}/{}/models/{}:streamGenerateContent?alt=sse&key={}",
            self.base_url.trim_end_matches('/'),
            API_VERSION,
            req.model,
            self.api_key,
        );
        let response = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| ArccodeError::Provider(format!("gemini request: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(ArccodeError::Provider(format!(
                "gemini returned {status}: {text}"
            )));
        }

        let bytes = response.bytes_stream();
        let mut events = bytes.eventsource();
        let mut next_tool_idx = 0u32;

        let stream = async_stream::try_stream! {
            while let Some(item) = events.next().await {
                let evt = match item {
                    Ok(e) => e,
                    Err(e) => Err(ArccodeError::Provider(format!("sse: {e}")))?,
                };
                if evt.data.is_empty() { continue; }
                let chunk: Value = match serde_json::from_str(&evt.data) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(target: "arccode::gemini", "bad sse json: {e}: {}", evt.data);
                        continue;
                    }
                };

                if let Some(usage) = chunk.get("usageMetadata").and_then(parse_usage) {
                    yield StreamEvent::Usage { usage };
                }

                if let Some(parts) = chunk
                    .get("candidates")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("content"))
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array())
                {
                    for part in parts {
                        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                yield StreamEvent::TextDelta { text: text.to_string() };
                            }
                        }
                        if let Some(fc) = part.get("functionCall") {
                            let name = fc.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let args = fc.get("args").cloned().unwrap_or(Value::Object(Default::default()));
                            let id = format!("call_{}", next_tool_idx);
                            next_tool_idx += 1;
                            yield StreamEvent::ToolUse {
                                block: ContentBlock::ToolUse { id, name, input: args },
                            };
                        }
                    }
                }

                if let Some(reason) = chunk
                    .get("candidates")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("finishReason"))
                    .and_then(|v| v.as_str())
                {
                    let stop = match reason {
                        "STOP" => StopReason::EndTurn,
                        "MAX_TOKENS" => StopReason::MaxTokens,
                        "SAFETY" | "RECITATION" | "OTHER" => StopReason::Other,
                        _ => StopReason::Other,
                    };
                    yield StreamEvent::Stop { reason: stop };
                    break;
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

fn parse_usage(v: &Value) -> Option<Usage> {
    let field = |name: &str| v.get(name).and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    Some(Usage {
        input_tokens: field("promptTokenCount"),
        output_tokens: field("candidatesTokenCount"),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: field("cachedContentTokenCount"),
    })
}

fn build_request_body(req: &CompletionRequest) -> Value {
    let mut body = json!({
        "contents": encode_contents(&req.messages),
        "generationConfig": {
            "maxOutputTokens": req.max_tokens,
        },
    });
    if let Some(t) = req.temperature {
        body["generationConfig"]["temperature"] = json!(t);
    }
    if let Some(sys) = &req.system {
        body["systemInstruction"] = json!({
            "parts": [{ "text": sys }],
        });
    }
    if !req.tools.is_empty() {
        body["tools"] = json!([{ "functionDeclarations": encode_tools(&req.tools) }]);
    }
    body
}

fn encode_tools(tools: &[ToolSpec]) -> Value {
    let arr: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "parameters": clean_schema_for_gemini(&t.input_schema),
            })
        })
        .collect();
    Value::Array(arr)
}

/// Gemini's OpenAPI-subset doesn't accept `additionalProperties` and a few
/// other JSON Schema fields. Strip them recursively.
fn clean_schema_for_gemini(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                if matches!(k.as_str(), "additionalProperties" | "$schema" | "default") {
                    continue;
                }
                out.insert(k.clone(), clean_schema_for_gemini(val));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(clean_schema_for_gemini).collect()),
        _ => v.clone(),
    }
}

fn encode_contents(messages: &[Message]) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        // Tool-results from a "user" role message map to model `function`
        // role responses (Gemini calls this `functionResponse` parts).
        if m.role == Role::User
            && m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        {
            let parts: Vec<Value> = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult {
                        tool_use_id: _,
                        content,
                        ..
                    } => Some(json!({
                        "functionResponse": {
                            "name": "tool",
                            "response": { "content": content }
                        }
                    })),
                    _ => None,
                })
                .collect();
            out.push(json!({ "role": "user", "parts": parts }));
            continue;
        }

        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "model",
        };
        let parts: Vec<Value> = m
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => json!({ "text": text }),
                ContentBlock::ToolUse { name, input, .. } => json!({
                    "functionCall": { "name": name, "args": input }
                }),
                ContentBlock::ToolResult { .. } => json!({}),
                ContentBlock::Image { data, media_type } => json!({
                    "inline_data": {
                        "mime_type": media_type,
                        "data": data,
                    }
                }),
            })
            .collect();
        out.push(json!({ "role": role, "parts": parts }));
    }
    Value::Array(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_becomes_system_instruction() {
        let mut req = CompletionRequest::new("gemini-2.5-pro");
        req.system = Some("hi".into());
        let body = build_request_body(&req);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "hi");
    }

    #[test]
    fn tools_become_function_declarations() {
        let mut req = CompletionRequest::new("gemini-2.5-pro");
        req.tools = vec![ToolSpec {
            name: "foo".into(),
            description: "do".into(),
            input_schema: json!({"type":"object", "additionalProperties": false}),
        }];
        let body = build_request_body(&req);
        let decl = &body["tools"][0]["functionDeclarations"][0];
        assert_eq!(decl["name"], "foo");
        assert!(decl["parameters"].get("additionalProperties").is_none());
    }

    #[test]
    fn assistant_role_renames_to_model() {
        let mut req = CompletionRequest::new("gemini-2.5-pro");
        req.messages = vec![Message::assistant(vec![ContentBlock::text("hi")])];
        let body = build_request_body(&req);
        assert_eq!(body["contents"][0]["role"], "model");
    }

    #[test]
    fn function_call_maps_to_part_function_call() {
        let mut req = CompletionRequest::new("gemini-2.5-pro");
        req.messages = vec![Message::assistant(vec![ContentBlock::ToolUse {
            id: "x".into(),
            name: "foo".into(),
            input: json!({"a":1}),
        }])];
        let body = build_request_body(&req);
        assert_eq!(
            body["contents"][0]["parts"][0]["functionCall"]["name"],
            "foo"
        );
        assert_eq!(
            body["contents"][0]["parts"][0]["functionCall"]["args"]["a"],
            1
        );
    }
}
