//! IBM watsonx.ai chat completions provider.
//!
//! Endpoint: `POST /ml/v1/text/chat_stream?version=2023-05-29` against the
//! region-specific host (e.g. `https://us-south.ml.cloud.ibm.com`,
//! `https://eu-de.ml.cloud.ibm.com`).
//!
//! ## Auth
//!
//! Watsonx requires a bearer **IAM access token**, not the raw API key.
//! IAM tokens are short-lived (~1h) so this adapter handles the exchange
//! internally: it POSTs the `WATSONX_API_KEY` to IBM's IAM endpoint
//! (`https://iam.cloud.ibm.com/identity/token`) on first use and caches
//! the access token in memory, refreshing ~60s before expiry.
//!
//! If the user has already obtained a token via some other flow they can
//! pass it in instead and skip the exchange.
//!
//! ## Body shape
//!
//! Close to OpenAI Chat Completions but with three required differences:
//! `model_id` instead of `model`, mandatory `project_id`, and an optional
//! `time_limit` field instead of `max_tokens` for the request budget.
//! The streaming response is SSE in OpenAI shape (`choices[0].delta`)
//! so we reuse the same chunk parser as the OpenAI-compat provider.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use wingman_core::{
    WingmanError, CacheKind, CompletionRequest, ContentBlock, Message, Provider,
    ProviderCapabilities, ProviderEventStream, Result, Role, StopReason, StreamEvent, ToolSpec,
    Usage,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use serde_json::{json, Value};

const DEFAULT_BASE_URL: &str = "https://us-south.ml.cloud.ibm.com";
const DEFAULT_IAM_URL: &str = "https://iam.cloud.ibm.com/identity/token";
const WATSONX_VERSION: &str = "2023-05-29";

/// Either an IBM Cloud API key (we'll exchange it for an IAM token) or a
/// pre-obtained IAM access token (used as-is).
#[derive(Debug, Clone)]
pub enum WatsonxCredential {
    ApiKey(String),
    AccessToken(String),
}

#[derive(Debug)]
pub struct WatsonxProvider {
    credential: WatsonxCredential,
    project_id: String,
    base_url: String,
    iam_url: String,
    http: reqwest::Client,
    /// Cached `(token, expires_at)`. Populated lazily; only used when
    /// `credential` is `ApiKey`.
    cached_token: Mutex<Option<(String, Instant)>>,
}

impl WatsonxProvider {
    pub fn new(credential: WatsonxCredential, project_id: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| WingmanError::Provider(format!("http client: {e}")))?;
        Ok(Self {
            credential,
            project_id: project_id.into(),
            base_url: DEFAULT_BASE_URL.into(),
            iam_url: DEFAULT_IAM_URL.into(),
            http,
            cached_token: Mutex::new(None),
        })
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_iam_url(mut self, iam_url: impl Into<String>) -> Self {
        self.iam_url = iam_url.into();
        self
    }

    /// Resolve a usable bearer token. For `AccessToken` credentials this
    /// is a no-op; for `ApiKey` it hits the IBM IAM endpoint (with an
    /// in-process cache) and exchanges the key for an access token.
    async fn bearer(&self) -> Result<String> {
        match &self.credential {
            WatsonxCredential::AccessToken(t) => Ok(t.clone()),
            WatsonxCredential::ApiKey(key) => {
                if let Some((tok, exp)) = self.cached_token.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                    if exp > Instant::now() {
                        return Ok(tok);
                    }
                }
                let form = [
                    ("grant_type", "urn:ibm:params:oauth:grant-type:apikey"),
                    ("apikey", key.as_str()),
                ];
                let resp = self
                    .http
                    .post(&self.iam_url)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("accept", "application/json")
                    .form(&form)
                    .send()
                    .await
                    .map_err(|e| WingmanError::Provider(format!("watsonx iam: {e}")))?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    return Err(WingmanError::Provider(format!(
                        "watsonx iam returned {status}: {text}"
                    )));
                }
                let body: Value = resp
                    .json()
                    .await
                    .map_err(|e| WingmanError::Provider(format!("watsonx iam decode: {e}")))?;
                let token = body
                    .get("access_token")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        WingmanError::Provider("watsonx iam: missing access_token".into())
                    })?
                    .to_string();
                let lifetime_secs = body
                    .get("expires_in")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(3600);
                // Refresh 60s before the IAM token expires.
                let exp = Instant::now() + Duration::from_secs(lifetime_secs.saturating_sub(60));
                *self.cached_token.lock().unwrap_or_else(|e| e.into_inner()) = Some((token.clone(), exp));
                Ok(token)
            }
        }
    }
}

#[async_trait]
impl Provider for WatsonxProvider {
    fn id(&self) -> &str {
        "watsonx"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tools: true,
            vision: false,
            cache_kind: CacheKind::Automatic,
        }
    }

    async fn complete(&self, req: CompletionRequest) -> Result<ProviderEventStream> {
        let body = build_request_body(&req, &self.project_id);
        tracing::debug!(target: "wingman::watsonx", "request: {body}");

        let token = self.bearer().await?;
        let url = format!(
            "{}/ml/v1/text/chat_stream?version={}",
            self.base_url.trim_end_matches('/'),
            WATSONX_VERSION,
        );
        let response = crate::retry::send_with_retry("watsonx", || {
            self.http
                .post(&url)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .header("authorization", format!("Bearer {token}"))
                .json(&body)
                .send()
        })
        .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(WingmanError::Provider(format!(
                "watsonx returned {status}: {text}"
            )));
        }

        let bytes = response.bytes_stream();
        let mut events = bytes.eventsource();

        // Watsonx streams OpenAI-shape `choices[0].delta` chunks, so the
        // same accumulator pattern as `openai_compat` works.
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
        // Suppress a second Stop on `[DONE]` when `finish_reason` already
        // emitted one — otherwise consumers see two Stop events per turn.
        let mut stop_emitted = false;

        let stream = async_stream::try_stream! {
            while let Some(item) = events.next().await {
                let evt = match item {
                    Ok(e) => e,
                    Err(e) => Err(WingmanError::Provider(format!("sse: {e}")))?,
                };
                if evt.data.is_empty() { continue; }
                if evt.data.trim() == "[DONE]" {
                    for (idx, acc) in tool_accs.drain() {
                        if emitted_tool_indices.contains(&idx) { continue; }
                        let input: Value = serde_json::from_str(&acc.args).unwrap_or_else(|_| Value::Object(Default::default()));
                        yield StreamEvent::ToolUse {
                            block: ContentBlock::ToolUse { id: acc.id, name: acc.name, input },
                        };
                    }
                    if !stop_emitted {
                        yield StreamEvent::Stop { reason: StopReason::EndTurn };
                    }
                    break;
                }
                let chunk: Value = match serde_json::from_str(&evt.data) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(target: "wingman::watsonx", "bad sse json: {e}: {}", evt.data);
                        continue;
                    }
                };
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
                    stop_emitted = true;
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
        cache_read_input_tokens: 0,
    })
}

fn build_request_body(req: &CompletionRequest, project_id: &str) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = &req.system {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for m in &req.messages {
        encode_message(m, &mut messages);
    }

    let mut body = json!({
        "model_id": req.model,
        "project_id": project_id,
        "messages": messages,
        // Watsonx accepts `max_tokens` (OpenAI alias) as well as
        // `time_limit` (ms); we use the token cap for parity with the
        // rest of wingman.
        "max_tokens": req.max_tokens,
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
    let mut tool_calls: Vec<Value> = Vec::new();
    for b in &m.content {
        match b {
            ContentBlock::Text { text: t } => text.push_str(t),
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": input.to_string() }
                }));
            }
            ContentBlock::ToolResult { .. } => {}
            ContentBlock::Image { .. } => {}
        }
    }

    let mut msg = json!({ "role": role });
    if !text.is_empty() {
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
    fn body_includes_project_and_model_id() {
        let req = CompletionRequest::new("ibm/granite-3-8b-instruct");
        let body = build_request_body(&req, "my-project");
        assert_eq!(body["model_id"], "ibm/granite-3-8b-instruct");
        assert_eq!(body["project_id"], "my-project");
        assert!(body.get("messages").is_some());
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
        encode_message(
            &Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
            },
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["tool_call_id"], "call_1");
    }
}
