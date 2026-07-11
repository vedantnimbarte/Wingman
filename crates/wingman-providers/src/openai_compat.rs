//! OpenAI-compatible chat completions provider.
//!
//! One adapter covers the OpenAI Chat Completions API and every API-shape
//! clone that wingman supports. Hosted clouds: OpenAI, OpenRouter, Groq,
//! Together AI, Fireworks, DeepInfra, Perplexity, xAI (Grok), DeepSeek,
//! Mistral La Plateforme, Cerebras, SambaNova, Azure OpenAI, GitHub Models.
//! Self-hosted / local: LM Studio, vLLM, LiteLLM proxy, Ollama, llama.cpp
//! server, HuggingFace TGI. The provider id and default base URL are
//! picked up from [`Variant`].
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

use wingman_core::{
    WingmanError, CacheKind, CompletionRequest, ContentBlock, Message, Provider,
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
    Groq,
    Together,
    Fireworks,
    DeepInfra,
    Perplexity,
    XAI,
    DeepSeek,
    Mistral,
    Cerebras,
    SambaNova,
    AzureOpenAI,
    GithubModels,
    LlamaCpp,
    Tgi,
    // Wave 2: hosted OpenAI-shape clouds.
    Anyscale,
    Lepton,
    Replicate,
    Novita,
    Hyperbolic,
    Lambda,
    Nebius,
    HfInference,
    Glhf,
    Featherless,
    OctoAi,
    NvidiaNim,
    Avian,
    Kluster,
    InferenceNet,
    Snowflake,
    Databricks,
    Writer,
    // Wave 2: local runtimes.
    Gpt4All,
    Jan,
    KoboldCpp,
    Oobabooga,
    // Wave 3: Chinese hosted clouds (all OpenAI-shape).
    DashScope,
    Zhipu,
    Moonshot,
    MiniMax,
    Yi,
    Baichuan,
    Hunyuan,
    Doubao,
    SiliconFlow,
    // Wave 3: aggregators / gateways.
    Cloudflare,
    Vercel,
    AimlApi,
    OpenPipe,
    Targon,
    Pollinations,
    // Wave 3: local runtimes.
    MlxLm,
    LocalAi,
    Aphrodite,
    MistralRs,
    // Wave 3: other hosted.
    Ai21,
    Zai,
    Friendli,
    Mancer,
    Reka,
    // Wave 4: enterprise clouds via OpenAI-compat surfaces.
    Bedrock,
    Vertex,
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
            Variant::Groq => "groq",
            Variant::Together => "together",
            Variant::Fireworks => "fireworks",
            Variant::DeepInfra => "deepinfra",
            Variant::Perplexity => "perplexity",
            Variant::XAI => "xai",
            Variant::DeepSeek => "deepseek",
            Variant::Mistral => "mistral",
            Variant::Cerebras => "cerebras",
            Variant::SambaNova => "sambanova",
            Variant::AzureOpenAI => "azure",
            Variant::GithubModels => "github",
            Variant::LlamaCpp => "llamacpp",
            Variant::Tgi => "tgi",
            Variant::Anyscale => "anyscale",
            Variant::Lepton => "lepton",
            Variant::Replicate => "replicate",
            Variant::Novita => "novita",
            Variant::Hyperbolic => "hyperbolic",
            Variant::Lambda => "lambda",
            Variant::Nebius => "nebius",
            Variant::HfInference => "hf",
            Variant::Glhf => "glhf",
            Variant::Featherless => "featherless",
            Variant::OctoAi => "octoai",
            Variant::NvidiaNim => "nvidia",
            Variant::Avian => "avian",
            Variant::Kluster => "kluster",
            Variant::InferenceNet => "inferencenet",
            Variant::Snowflake => "snowflake",
            Variant::Databricks => "databricks",
            Variant::Writer => "writer",
            Variant::Gpt4All => "gpt4all",
            Variant::Jan => "jan",
            Variant::KoboldCpp => "koboldcpp",
            Variant::Oobabooga => "oobabooga",
            Variant::DashScope => "qwen",
            Variant::Zhipu => "zhipu",
            Variant::Moonshot => "moonshot",
            Variant::MiniMax => "minimax",
            Variant::Yi => "yi",
            Variant::Baichuan => "baichuan",
            Variant::Hunyuan => "hunyuan",
            Variant::Doubao => "doubao",
            Variant::SiliconFlow => "siliconflow",
            Variant::Cloudflare => "cloudflare",
            Variant::Vercel => "vercel",
            Variant::AimlApi => "aimlapi",
            Variant::OpenPipe => "openpipe",
            Variant::Targon => "targon",
            Variant::Pollinations => "pollinations",
            Variant::MlxLm => "mlx",
            Variant::LocalAi => "localai",
            Variant::Aphrodite => "aphrodite",
            Variant::MistralRs => "mistralrs",
            Variant::Ai21 => "ai21",
            Variant::Zai => "zai",
            Variant::Friendli => "friendli",
            Variant::Mancer => "mancer",
            Variant::Reka => "reka",
            Variant::Bedrock => "bedrock",
            Variant::Vertex => "vertex",
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
            Variant::Groq => "https://api.groq.com/openai/v1",
            Variant::Together => "https://api.together.xyz/v1",
            Variant::Fireworks => "https://api.fireworks.ai/inference/v1",
            Variant::DeepInfra => "https://api.deepinfra.com/v1/openai",
            Variant::Perplexity => "https://api.perplexity.ai",
            Variant::XAI => "https://api.x.ai/v1",
            Variant::DeepSeek => "https://api.deepseek.com/v1",
            Variant::Mistral => "https://api.mistral.ai/v1",
            Variant::Cerebras => "https://api.cerebras.ai/v1",
            Variant::SambaNova => "https://api.sambanova.ai/v1",
            // Azure OpenAI: users MUST override with their resource URL,
            // e.g. https://<resource>.openai.azure.com/openai/deployments/<deployment>.
            Variant::AzureOpenAI => "https://example.openai.azure.com/openai/deployments/REPLACE-ME",
            Variant::GithubModels => "https://models.inference.ai.azure.com",
            Variant::LlamaCpp => "http://localhost:8080/v1",
            Variant::Tgi => "http://localhost:3000/v1",
            Variant::Anyscale => "https://api.endpoints.anyscale.com/v1",
            Variant::Lepton => "https://api.lepton.ai/api/v1",
            Variant::Replicate => "https://openai-proxy.replicate.com/v1",
            Variant::Novita => "https://api.novita.ai/v3/openai",
            Variant::Hyperbolic => "https://api.hyperbolic.xyz/v1",
            Variant::Lambda => "https://api.lambdalabs.com/v1",
            Variant::Nebius => "https://api.studio.nebius.ai/v1",
            Variant::HfInference => "https://router.huggingface.co/v1",
            Variant::Glhf => "https://glhf.chat/api/openai/v1",
            Variant::Featherless => "https://api.featherless.ai/v1",
            Variant::OctoAi => "https://text.octoai.run/v1",
            Variant::NvidiaNim => "https://integrate.api.nvidia.com/v1",
            Variant::Avian => "https://api.avian.io/v1",
            Variant::Kluster => "https://api.kluster.ai/v1",
            Variant::InferenceNet => "https://api.inference.net/v1",
            // Snowflake Cortex: user must override with their account URL,
            // e.g. https://<account>.snowflakecomputing.com/api/v2/cortex/inference.
            Variant::Snowflake => "https://example.snowflakecomputing.com/api/v2/cortex/inference/v1",
            // Databricks: user must override with their workspace URL,
            // e.g. https://<workspace>.cloud.databricks.com/serving-endpoints.
            Variant::Databricks => "https://example.cloud.databricks.com/serving-endpoints/v1",
            Variant::Writer => "https://api.writer.com/v1",
            Variant::Gpt4All => "http://localhost:4891/v1",
            Variant::Jan => "http://localhost:1337/v1",
            Variant::KoboldCpp => "http://localhost:5001/v1",
            Variant::Oobabooga => "http://localhost:5000/v1",
            // Alibaba DashScope international endpoint; users in mainland China
            // may override with the regional one (.aliyuncs.com without "intl").
            Variant::DashScope => "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
            Variant::Zhipu => "https://open.bigmodel.cn/api/paas/v4",
            Variant::Moonshot => "https://api.moonshot.cn/v1",
            Variant::MiniMax => "https://api.minimaxi.com/v1",
            Variant::Yi => "https://api.lingyiwanwu.com/v1",
            Variant::Baichuan => "https://api.baichuan-ai.com/v1",
            Variant::Hunyuan => "https://api.hunyuan.cloud.tencent.com/v1",
            Variant::Doubao => "https://ark.cn-beijing.volces.com/api/v3",
            Variant::SiliconFlow => "https://api.siliconflow.cn/v1",
            // Cloudflare requires an account id in the path; user must override.
            Variant::Cloudflare => {
                "https://api.cloudflare.com/client/v4/accounts/REPLACE-ME/ai/v1"
            }
            Variant::Vercel => "https://gateway.ai.vercel.com/v1",
            Variant::AimlApi => "https://api.aimlapi.com/v1",
            Variant::OpenPipe => "https://api.openpipe.ai/api/v1",
            Variant::Targon => "https://api.targon.com/v1",
            Variant::Pollinations => "https://text.pollinations.ai/openai/v1",
            Variant::MlxLm => "http://localhost:8080/v1",
            Variant::LocalAi => "http://localhost:8080/v1",
            Variant::Aphrodite => "http://localhost:2242/v1",
            Variant::MistralRs => "http://localhost:1234/v1",
            Variant::Ai21 => "https://api.ai21.com/studio/v1",
            Variant::Zai => "https://api.z.ai/api/coding/paas/v4",
            Variant::Friendli => "https://inference.friendli.ai/v1",
            Variant::Mancer => "https://neuro.mancer.tech/oai/v1",
            Variant::Reka => "https://api.reka.ai/v1",
            // Bedrock OpenAI-compat endpoint (released 2024). User must
            // override the region. Auth: AWS_BEARER_TOKEN_BEDROCK (long-term
            // Bedrock API key generated from the AWS console). The SigV4
            // path against `/model/<id>/invoke-with-response-stream` would
            // need a dedicated adapter — not done in this provider.
            Variant::Bedrock => "https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1",
            // Vertex AI's OpenAI-compatible endpoint. User must override
            // base_url with their project_id + region. Auth: bearer access
            // token from `gcloud auth print-access-token` (expires hourly).
            Variant::Vertex => {
                "https://us-central1-aiplatform.googleapis.com/v1/projects/REPLACE-PROJECT/locations/us-central1/endpoints/openapi"
            }
        }
    }

    /// Whether a real API key is mandatory for this variant. Local
    /// providers accept any string (or none) and we send a dummy token.
    pub fn requires_api_key(self) -> bool {
        matches!(
            self,
            Variant::OpenAI
                | Variant::OpenRouter
                | Variant::Groq
                | Variant::Together
                | Variant::Fireworks
                | Variant::DeepInfra
                | Variant::Perplexity
                | Variant::XAI
                | Variant::DeepSeek
                | Variant::Mistral
                | Variant::Cerebras
                | Variant::SambaNova
                | Variant::AzureOpenAI
                | Variant::GithubModels
                | Variant::Anyscale
                | Variant::Lepton
                | Variant::Replicate
                | Variant::Novita
                | Variant::Hyperbolic
                | Variant::Lambda
                | Variant::Nebius
                | Variant::HfInference
                | Variant::Glhf
                | Variant::Featherless
                | Variant::OctoAi
                | Variant::NvidiaNim
                | Variant::Avian
                | Variant::Kluster
                | Variant::InferenceNet
                | Variant::Snowflake
                | Variant::Databricks
                | Variant::Writer
                | Variant::DashScope
                | Variant::Zhipu
                | Variant::Moonshot
                | Variant::MiniMax
                | Variant::Yi
                | Variant::Baichuan
                | Variant::Hunyuan
                | Variant::Doubao
                | Variant::SiliconFlow
                | Variant::Cloudflare
                | Variant::Vercel
                | Variant::AimlApi
                | Variant::OpenPipe
                | Variant::Targon
                | Variant::Ai21
                | Variant::Zai
                | Variant::Friendli
                | Variant::Mancer
                | Variant::Reka
                | Variant::Bedrock
                | Variant::Vertex
        )
    }

    /// Azure OpenAI is OpenAI-shape but uses `api-key:` header (not Bearer)
    /// and a deployment-name path; we keep the same `/chat/completions`
    /// suffix and let the user point `base_url` at their deployment.
    fn uses_api_key_header(self) -> bool {
        matches!(self, Variant::AzureOpenAI)
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
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| WingmanError::Provider(format!("http client: {e}")))?;
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
            vision: matches!(
                self.variant,
                Variant::OpenAI
                    | Variant::OpenRouter
                    | Variant::Together
                    | Variant::Fireworks
                    | Variant::XAI
                    | Variant::Mistral
                    | Variant::AzureOpenAI
                    | Variant::GithubModels
                    | Variant::Hyperbolic
                    | Variant::Nebius
                    | Variant::HfInference
                    | Variant::NvidiaNim
                    | Variant::DeepInfra
                    | Variant::Novita
                    | Variant::DashScope
                    | Variant::Zhipu
                    | Variant::MiniMax
                    | Variant::Doubao
                    | Variant::SiliconFlow
                    | Variant::Cloudflare
                    | Variant::Vercel
                    | Variant::AimlApi
                    | Variant::Reka
                    | Variant::Bedrock
                    | Variant::Vertex
            ),
            cache_kind: CacheKind::Automatic,
        }
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let mut builder = self.http.get(&url).header("accept", "application/json");

        // Same auth scheme as `complete`.
        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                if self.variant.uses_api_key_header() {
                    builder = builder.header("api-key", key.as_str());
                } else {
                    builder = builder.header("authorization", format!("Bearer {key}"));
                }
            }
        } else if self.variant.requires_api_key() {
            return Err(WingmanError::Provider(format!(
                "provider {} requires an api_key",
                self.variant.id()
            )));
        }
        if matches!(self.variant, Variant::OpenRouter) {
            builder = builder
                .header("HTTP-Referer", "https://github.com/your-org/wingman")
                .header("X-Title", "wingman");
        }

        let response = builder
            .send()
            .await
            .map_err(|e| WingmanError::Provider(format!("list models request: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(WingmanError::Provider(format!(
                "{} /models returned {status}: {text}",
                self.variant.id()
            )));
        }

        let json: Value = response
            .json()
            .await
            .map_err(|e| WingmanError::Provider(format!("list models parse: {e}")))?;
        // Standard OpenAI shape: `{ "data": [ { "id": "…" }, … ] }`.
        let ids: Vec<String> = json
            .get("data")
            .and_then(|d| d.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("id").and_then(|s| s.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok(ids)
    }

    async fn complete(&self, req: CompletionRequest) -> Result<ProviderEventStream> {
        let body = build_request_body(&req);
        tracing::debug!(target: "wingman::openai_compat", "request: {body}");

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut builder = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body);

        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                if self.variant.uses_api_key_header() {
                    builder = builder.header("api-key", key.as_str());
                } else {
                    builder = builder.header("authorization", format!("Bearer {key}"));
                }
            }
        } else if self.variant.requires_api_key() {
            return Err(WingmanError::Provider(format!(
                "provider {} requires an api_key",
                self.variant.id()
            )));
        }

        // OpenRouter best-practice headers (harmless elsewhere).
        if matches!(self.variant, Variant::OpenRouter) {
            builder = builder
                .header("HTTP-Referer", "https://github.com/your-org/wingman")
                .header("X-Title", "wingman");
        }

        let label = self.variant.id();
        let response = crate::retry::send_with_retry(label, || {
            builder
                .try_clone()
                .expect("json request body is always cloneable")
                .send()
        })
        .await?;

        let status = response.status();

        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(WingmanError::Provider(format!(
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
        // Track whether we already emitted a Stop (on `finish_reason`) so the
        // `[DONE]` sentinel doesn't yield a second one — consumers otherwise
        // see two Stop events per turn.
        let mut stop_emitted = false;

        let stream = async_stream::try_stream! {
            while let Some(item) = events.next().await {
                let evt = match item {
                    Ok(e) => e,
                    Err(e) => Err(WingmanError::Provider(format!("sse: {e}")))?,
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
                    if !stop_emitted {
                        yield StreamEvent::Stop { reason: StopReason::EndTurn };
                    }
                    break;
                }
                let chunk: Value = match serde_json::from_str(&evt.data) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(target: "wingman::openai_compat", "bad sse json: {e}: {}", evt.data);
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
                    stop_emitted = true;
                    // Keep reading for trailing usage + [DONE].
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

fn parse_usage(v: &Value) -> Option<Usage> {
    let field = |name: &str| v.get(name).and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    // `prompt_tokens` includes the cached slice; subtract it so cost isn't
    // billed twice (full input rate + cache-read rate).
    let cached = v
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    Some(Usage {
        input_tokens: field("prompt_tokens").saturating_sub(cached),
        output_tokens: field("completion_tokens"),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
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
        "stream": true,
        "messages": messages,
        "stream_options": { "include_usage": true },
    });
    // OpenAI reasoning models (o-series, gpt-5 family) reject `max_tokens`
    // (they require `max_completion_tokens`) and reject any non-default
    // `temperature`. Emitting the legacy fields makes every such model 400.
    if is_reasoning_model(&req.model) {
        body["max_completion_tokens"] = json!(req.max_tokens);
    } else {
        body["max_tokens"] = json!(req.max_tokens);
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
    }
    if !req.tools.is_empty() {
        body["tools"] = encode_tools(&req.tools);
    }
    body
}

/// True for OpenAI reasoning-style model ids that need `max_completion_tokens`
/// instead of `max_tokens` and reject a custom `temperature`.
///
/// ponytail: name-based heuristic (o1/o3/o4/gpt-5). A local server hosting a
/// model literally named `o1` would be misrouted; upgrade to a per-variant
/// capability flag if that ever bites.
fn is_reasoning_model(model: &str) -> bool {
    let m = model.rsplit('/').next().unwrap_or(model).to_ascii_lowercase();
    m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
        || m.starts_with("gpt-5")
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
        let all = [
            Variant::OpenAI,
            Variant::OpenRouter,
            Variant::LmStudio,
            Variant::Vllm,
            Variant::LiteLlm,
            Variant::Ollama,
            Variant::Groq,
            Variant::Together,
            Variant::Fireworks,
            Variant::DeepInfra,
            Variant::Perplexity,
            Variant::XAI,
            Variant::DeepSeek,
            Variant::Mistral,
            Variant::Cerebras,
            Variant::SambaNova,
            Variant::AzureOpenAI,
            Variant::GithubModels,
            Variant::LlamaCpp,
            Variant::Tgi,
            Variant::Anyscale,
            Variant::Lepton,
            Variant::Replicate,
            Variant::Novita,
            Variant::Hyperbolic,
            Variant::Lambda,
            Variant::Nebius,
            Variant::HfInference,
            Variant::Glhf,
            Variant::Featherless,
            Variant::OctoAi,
            Variant::NvidiaNim,
            Variant::Avian,
            Variant::Kluster,
            Variant::InferenceNet,
            Variant::Snowflake,
            Variant::Databricks,
            Variant::Writer,
            Variant::Gpt4All,
            Variant::Jan,
            Variant::KoboldCpp,
            Variant::Oobabooga,
            Variant::DashScope,
            Variant::Zhipu,
            Variant::Moonshot,
            Variant::MiniMax,
            Variant::Yi,
            Variant::Baichuan,
            Variant::Hunyuan,
            Variant::Doubao,
            Variant::SiliconFlow,
            Variant::Cloudflare,
            Variant::Vercel,
            Variant::AimlApi,
            Variant::OpenPipe,
            Variant::Targon,
            Variant::Pollinations,
            Variant::MlxLm,
            Variant::LocalAi,
            Variant::Aphrodite,
            Variant::MistralRs,
            Variant::Ai21,
            Variant::Zai,
            Variant::Friendli,
            Variant::Mancer,
            Variant::Reka,
            Variant::Bedrock,
            Variant::Vertex,
        ];
        let mut seen = std::collections::HashSet::new();
        for v in all {
            assert!(!v.id().is_empty(), "variant has empty id");
            assert!(v.default_base_url().starts_with("http"));
            assert!(seen.insert(v.id()), "duplicate variant id {}", v.id());
        }
    }

    #[test]
    fn hosted_variants_require_api_key() {
        for v in [
            Variant::Groq,
            Variant::Together,
            Variant::Fireworks,
            Variant::DeepInfra,
            Variant::Perplexity,
            Variant::XAI,
            Variant::DeepSeek,
            Variant::Mistral,
            Variant::Cerebras,
            Variant::SambaNova,
            Variant::AzureOpenAI,
            Variant::GithubModels,
            Variant::Anyscale,
            Variant::Lepton,
            Variant::Replicate,
            Variant::Novita,
            Variant::Hyperbolic,
            Variant::Lambda,
            Variant::Nebius,
            Variant::HfInference,
            Variant::Glhf,
            Variant::Featherless,
            Variant::OctoAi,
            Variant::NvidiaNim,
            Variant::Avian,
            Variant::Kluster,
            Variant::InferenceNet,
            Variant::Snowflake,
            Variant::Databricks,
            Variant::Writer,
            Variant::DashScope,
            Variant::Zhipu,
            Variant::Moonshot,
            Variant::MiniMax,
            Variant::Yi,
            Variant::Baichuan,
            Variant::Hunyuan,
            Variant::Doubao,
            Variant::SiliconFlow,
            Variant::Cloudflare,
            Variant::Vercel,
            Variant::AimlApi,
            Variant::OpenPipe,
            Variant::Targon,
            Variant::Ai21,
            Variant::Zai,
            Variant::Friendli,
            Variant::Mancer,
            Variant::Reka,
            Variant::Bedrock,
            Variant::Vertex,
        ] {
            assert!(v.requires_api_key(), "{} should require api key", v.id());
        }
    }

    #[test]
    fn local_variants_do_not_require_api_key() {
        for v in [
            Variant::LmStudio,
            Variant::Vllm,
            Variant::Ollama,
            Variant::LiteLlm,
            Variant::LlamaCpp,
            Variant::Tgi,
            Variant::Gpt4All,
            Variant::Jan,
            Variant::KoboldCpp,
            Variant::Oobabooga,
            Variant::Pollinations,
            Variant::MlxLm,
            Variant::LocalAi,
            Variant::Aphrodite,
            Variant::MistralRs,
        ] {
            assert!(
                !v.requires_api_key(),
                "{} should not require api key",
                v.id()
            );
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

    #[test]
    fn cached_tokens_not_double_counted() {
        // prompt_tokens includes the cached slice; input_tokens must exclude it.
        let u = parse_usage(&json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_tokens_details": { "cached_tokens": 800 },
        }))
        .unwrap();
        assert_eq!(u.input_tokens, 200);
        assert_eq!(u.cache_read_input_tokens, 800);
        assert_eq!(u.output_tokens, 50);
    }

    #[test]
    fn reasoning_model_detection() {
        for m in ["o1", "o1-mini", "o3", "o3-mini", "o4-mini", "gpt-5", "openai/o3-mini"] {
            assert!(is_reasoning_model(m), "{m} should be reasoning");
        }
        for m in ["gpt-4o", "gpt-4.1", "claude-opus-4-8", "llama-3", "o-something"] {
            assert!(!is_reasoning_model(m), "{m} should not be reasoning");
        }
    }

    #[test]
    fn reasoning_model_uses_max_completion_tokens_and_drops_temperature() {
        let mut req = CompletionRequest::new("o3-mini");
        req.temperature = Some(0.2);
        let body = build_request_body(&req);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["max_completion_tokens"], json!(req.max_tokens));
        assert!(body.get("temperature").is_none(), "temperature must be omitted");
    }

    #[test]
    fn non_reasoning_model_keeps_max_tokens_and_temperature() {
        let mut req = CompletionRequest::new("gpt-4o");
        req.temperature = Some(0.2);
        let body = build_request_body(&req);
        assert_eq!(body["max_tokens"], json!(req.max_tokens));
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(body["temperature"], json!(0.2_f32));
    }
}
