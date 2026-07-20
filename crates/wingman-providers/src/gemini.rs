//! Google Gemini provider via the native `generateContent` API.
//!
//! - Endpoint: `https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?alt=sse`.
//! - Streaming: SSE chunks each contain a partial response with
//!   `candidates[0].content.parts` to merge.
//! - Tool calling: tools are declared under `tools[0].functionDeclarations`;
//!   the model emits `parts[].functionCall { name, args }` blocks.
//!   Results come back via `parts[].functionResponse { name, response }`.
//! - Caching (`cachedContent`): the stable prefix (system prompt + tool
//!   declarations) is uploaded once per session as a `cachedContents`
//!   resource and referenced by name on every turn, so it isn't re-sent or
//!   re-billed at the full input rate. Creation is fail-open — any error
//!   (unsupported model, below the minimum-token floor, API failure) disables
//!   caching for the session and the request proceeds uncached.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use wingman_core::{
    CacheBreakpoint, CacheKind, CompletionRequest, ContentBlock, Message, Provider,
    ProviderCapabilities, ProviderEventStream, Result, Role, StopReason, StreamEvent, ToolSpec,
    Usage, WingmanError,
};

/// Gemini rejects `cachedContents` below a per-model minimum (~1024 tokens).
/// Gate on an approximate char count (~4 chars/token) with a safe margin so
/// we don't pay a creation round-trip that will only 400.
const CACHE_MIN_CHARS: usize = 8192;
/// TTL for a created cache resource. One hour comfortably outlives a session's
/// active turns; Gemini evicts it afterwards.
const CACHE_TTL: &str = "3600s";

/// Per-session `cachedContent` bookkeeping, shared across clones of the
/// provider (the agent loop holds one `Arc<dyn Provider>` for the session).
#[derive(Debug, Default)]
struct CacheState {
    /// prefix-hash → `cachedContents/<id>` resource name.
    map: std::collections::HashMap<u64, String>,
    /// Set once creation fails so we don't retry a failing call every turn.
    disabled: bool,
}
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
    cache: Arc<Mutex<CacheState>>,
}

impl GeminiProvider {
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
            cache: Arc::new(Mutex::new(CacheState::default())),
        })
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Return a `cachedContents` resource name for the given stable prefix,
    /// creating it on first use and reusing it thereafter. Fail-open: any
    /// error disables caching for the rest of the session and returns `None`
    /// so the caller sends the prefix inline as usual.
    async fn ensure_cache(&self, model: &str, system: &str, tools: &[ToolSpec]) -> Option<String> {
        let key = prefix_hash(model, system, tools);
        {
            let st = self.cache.lock().unwrap();
            if st.disabled {
                return None;
            }
            if let Some(name) = st.map.get(&key) {
                return Some(name.clone());
            }
        }

        let mut cache_body = json!({
            "model": format!("models/{model}"),
            "ttl": CACHE_TTL,
        });
        if !system.is_empty() {
            cache_body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
        }
        if !tools.is_empty() {
            cache_body["tools"] = json!([{ "functionDeclarations": encode_tools(tools) }]);
        }

        let url = format!(
            "{}/{}/cachedContents",
            self.base_url.trim_end_matches('/'),
            API_VERSION,
        );
        let created = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("x-goog-api-key", &self.api_key)
            .json(&cache_body)
            .send()
            .await;

        let name = match created {
            Ok(r) if r.status().is_success() => r
                .json::<Value>()
                .await
                .ok()
                .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(String::from)),
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                tracing::warn!(
                    target: "wingman::gemini",
                    "cachedContents create {status}: {text}; caching disabled this session"
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    target: "wingman::gemini",
                    "cachedContents create error: {e}; caching disabled this session"
                );
                None
            }
        };

        let mut st = self.cache.lock().unwrap();
        match name {
            Some(n) => {
                st.map.insert(key, n.clone());
                Some(n)
            }
            None => {
                st.disabled = true;
                None
            }
        }
    }
}

/// The stable prefix (system + tools) worth caching, when a cache breakpoint
/// requests it and it clears Gemini's minimum-token floor. Returns `None`
/// otherwise so the request is sent uncached.
fn cacheable_prefix(req: &CompletionRequest) -> Option<(&str, &[ToolSpec])> {
    let wants = req.cache_breakpoints.iter().any(|b| {
        matches!(
            b,
            CacheBreakpoint::AfterSystem
                | CacheBreakpoint::AfterTools
                | CacheBreakpoint::AfterMessage(_)
        )
    });
    if !wants {
        return None;
    }
    let system = req.system.as_deref().unwrap_or("");
    let approx_chars = system.len()
        + req
            .tools
            .iter()
            .map(|t| t.name.len() + t.description.len() + t.input_schema.to_string().len())
            .sum::<usize>();
    if approx_chars < CACHE_MIN_CHARS {
        return None;
    }
    Some((system, req.tools.as_slice()))
}

/// Stable hash of the cacheable prefix, so the same session reuses one cache
/// resource across turns and a changed system/tools set makes a fresh one.
fn prefix_hash(model: &str, system: &str, tools: &[ToolSpec]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    model.hash(&mut h);
    system.hash(&mut h);
    for t in tools {
        t.name.hash(&mut h);
        t.description.hash(&mut h);
        t.input_schema.to_string().hash(&mut h);
    }
    h.finish()
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

    async fn list_models(&self) -> Result<Vec<String>> {
        // GET /v1beta/models -> { "models": [ { "name": "models/gemini-…",
        // "supportedGenerationMethods": [...] }, … ] }. Key goes in the
        // header (not `?key=`) so it can't leak into error/log URLs.
        let url = format!(
            "{}/{}/models?pageSize=1000",
            self.base_url.trim_end_matches('/'),
            API_VERSION,
        );
        let response = self
            .http
            .get(&url)
            .header("accept", "application/json")
            .header("x-goog-api-key", &self.api_key)
            .send()
            .await
            .map_err(|e| WingmanError::Provider(format!("gemini list models request: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(WingmanError::Provider(format!(
                "gemini /models returned {status}: {text}"
            )));
        }

        let json: Value = response
            .json()
            .await
            .map_err(|e| WingmanError::Provider(format!("gemini list models parse: {e}")))?;
        let ids: Vec<String> = json
            .get("models")
            .and_then(|d| d.as_array())
            .map(|a| {
                a.iter()
                    // Skip embedding / non-chat models.
                    .filter(|m| {
                        m.get("supportedGenerationMethods")
                            .and_then(|v| v.as_array())
                            .map(|methods| {
                                methods
                                    .iter()
                                    .any(|x| x.as_str() == Some("generateContent"))
                            })
                            .unwrap_or(false)
                    })
                    .filter_map(|m| m.get("name").and_then(|s| s.as_str()))
                    // `complete` builds `/models/{model}:…`, so store the bare id.
                    .map(|name| name.strip_prefix("models/").unwrap_or(name).to_string())
                    .collect()
            })
            .unwrap_or_default();
        Ok(ids)
    }

    async fn complete(&self, req: CompletionRequest) -> Result<ProviderEventStream> {
        // If a cache breakpoint asks for it and the stable prefix is large
        // enough, upload (or reuse) it as a cachedContents resource and refer
        // to it by name so system + tools aren't resent every turn.
        let cached_name = match cacheable_prefix(&req) {
            Some((system, tools)) => self.ensure_cache(&req.model, system, tools).await,
            None => None,
        };
        let body = build_request_body(&req, cached_name.as_deref());
        tracing::debug!(target: "wingman::gemini", "request: {body}");

        // Pass the key via the `x-goog-api-key` header rather than the `?key=`
        // query param. reqwest's error Display includes the request URL, so a
        // key in the query string would leak into error messages and logs on
        // any network failure.
        let url = format!(
            "{}/{}/models/{}:streamGenerateContent?alt=sse",
            self.base_url.trim_end_matches('/'),
            API_VERSION,
            req.model,
        );
        let response = crate::retry::send_with_retry("gemini", || {
            self.http
                .post(&url)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .header("x-goog-api-key", &self.api_key)
                .json(&body)
                .send()
        })
        .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(WingmanError::Provider(format!(
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
                    Err(e) => Err(WingmanError::Provider(format!("sse: {e}")))?,
                };
                if evt.data.is_empty() { continue; }
                let chunk: Value = match serde_json::from_str(&evt.data) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(target: "wingman::gemini", "bad sse json: {e}: {}", evt.data);
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
    // promptTokenCount includes the cached slice — subtract to avoid double
    // billing. thoughtsTokenCount (2.5 thinking models) is reasoning output,
    // billed at the output rate but not folded into candidatesTokenCount.
    let cached = field("cachedContentTokenCount");
    Some(Usage {
        input_tokens: field("promptTokenCount").saturating_sub(cached),
        output_tokens: field("candidatesTokenCount") + field("thoughtsTokenCount"),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
    })
}

fn build_request_body(req: &CompletionRequest, cached: Option<&str>) -> Value {
    let mut body = json!({
        "contents": encode_contents(&req.messages),
        "generationConfig": {
            "maxOutputTokens": req.max_tokens,
        },
    });
    if let Some(t) = req.temperature {
        body["generationConfig"]["temperature"] = json!(t);
    }
    if let Some(name) = cached {
        // The system prompt and tools now live in the cache resource and must
        // not be resent (Gemini rejects a request that both references a cache
        // and repeats its cached fields).
        body["cachedContent"] = json!(name);
    } else {
        if let Some(sys) = &req.system {
            body["systemInstruction"] = json!({
                "parts": [{ "text": sys }],
            });
        }
        if !req.tools.is_empty() {
            body["tools"] = json!([{ "functionDeclarations": encode_tools(&req.tools) }]);
        }
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
    // Gemini matches a functionResponse to its functionCall BY NAME, so a
    // result must carry the real function name — not a placeholder. Recover it
    // from the tool_use_id via the ToolUse block that requested the call.
    let mut name_by_id: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for m in messages {
        for b in &m.content {
            if let ContentBlock::ToolUse { id, name, .. } = b {
                name_by_id.insert(id.as_str(), name.as_str());
            }
        }
    }
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
                        tool_use_id,
                        content,
                        ..
                    } => {
                        let name = name_by_id
                            .get(tool_use_id.as_str())
                            .copied()
                            .unwrap_or("tool");
                        Some(json!({
                            "functionResponse": {
                                "name": name,
                                "response": { "content": content }
                            }
                        }))
                    }
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
        let body = build_request_body(&req, None);
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
        let body = build_request_body(&req, None);
        let decl = &body["tools"][0]["functionDeclarations"][0];
        assert_eq!(decl["name"], "foo");
        assert!(decl["parameters"].get("additionalProperties").is_none());
    }

    #[test]
    fn assistant_role_renames_to_model() {
        let mut req = CompletionRequest::new("gemini-2.5-pro");
        req.messages = vec![Message::assistant(vec![ContentBlock::text("hi")])];
        let body = build_request_body(&req, None);
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
        let body = build_request_body(&req, None);
        assert_eq!(
            body["contents"][0]["parts"][0]["functionCall"]["name"],
            "foo"
        );
        assert_eq!(
            body["contents"][0]["parts"][0]["functionCall"]["args"]["a"],
            1
        );
    }

    #[test]
    fn tool_result_carries_real_function_name() {
        // Assistant calls `read_file` (id "call-1"); the following user
        // tool_result must echo that name, not a "tool" placeholder, or Gemini
        // can't match the response to the call.
        let mut req = CompletionRequest::new("gemini-2.5-pro");
        req.messages = vec![
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "call-1".into(),
                name: "read_file".into(),
                input: json!({"path": "a.rs"}),
            }]),
            Message {
                role: wingman_core::Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "file contents".into(),
                    is_error: false,
                }],
            },
        ];
        let body = build_request_body(&req, None);
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["name"],
            "read_file"
        );
    }

    #[test]
    fn cached_body_omits_system_and_tools_and_sets_cached_content() {
        let mut req = CompletionRequest::new("gemini-2.5-pro");
        req.system = Some("big system prompt".into());
        req.tools = vec![ToolSpec {
            name: "foo".into(),
            description: "do".into(),
            input_schema: json!({"type":"object"}),
        }];
        let body = build_request_body(&req, Some("cachedContents/abc"));
        assert_eq!(body["cachedContent"], "cachedContents/abc");
        assert!(body.get("systemInstruction").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn cacheable_prefix_requires_breakpoint_and_floor() {
        let big = "x".repeat(CACHE_MIN_CHARS + 100);
        // No breakpoint → not cacheable even when large.
        let mut req = CompletionRequest::new("gemini-2.5-pro");
        req.system = Some(big.clone());
        assert!(cacheable_prefix(&req).is_none());
        // Breakpoint + large prefix → cacheable.
        req.cache_breakpoints = vec![CacheBreakpoint::AfterSystem];
        assert!(cacheable_prefix(&req).is_some());
        // Breakpoint but tiny prefix → below floor, not cacheable.
        req.system = Some("small".into());
        assert!(cacheable_prefix(&req).is_none());
    }

    #[test]
    fn prefix_hash_is_stable_and_sensitive() {
        let tools = vec![ToolSpec {
            name: "foo".into(),
            description: "do".into(),
            input_schema: json!({"type":"object"}),
        }];
        let a = prefix_hash("m", "sys", &tools);
        assert_eq!(
            a,
            prefix_hash("m", "sys", &tools),
            "same inputs → same hash"
        );
        assert_ne!(
            a,
            prefix_hash("m", "other", &tools),
            "system change → new hash"
        );
    }
}
