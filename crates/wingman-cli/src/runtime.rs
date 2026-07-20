//! Wires config into a concrete `Provider` + `ToolRegistry` + `AgentLoop`.
//!
//! Keeps the per-provider plumbing in one place so command handlers
//! (headless --print, --json, future TUI) can just ask `Runtime::build(...)`.

use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use wingman_config::{secrets, Config, PermissionMode, ProjectPaths};
use wingman_core::{
    AgentConfig, AgentLoop, Compactor, GateReport, Provider, ToolOutputBudget, TurnGate,
};
use wingman_learn::{
    hooks::{LearnConfig, LearnHandles},
    memory::MemoryStore,
};
use wingman_providers::{
    AnthropicProvider, ChatGptProvider, CohereProvider, GeminiProvider, OpenAiCompatProvider,
    OpenAiVariant, WatsonxCredential, WatsonxProvider,
};
use wingman_rag::{Embedder, HashEmbedder, IndexStore, Indexer};
use wingman_skills::Skill;
use wingman_tools::{ToolCtx, ToolRegistry};

#[derive(Debug, Clone)]
pub struct Selection {
    pub provider_id: String,
    pub model: String,
}

/// Parse a model string. Either `provider/model` (preferred) or bare
/// `model` (uses `default_provider` from config).
pub fn resolve_selection(cfg: &Config, model_flag: Option<&str>) -> Result<Selection> {
    let raw = model_flag
        .map(str::to_string)
        .or_else(|| cfg.default_model.clone())
        .or_else(|| {
            cfg.default_provider
                .as_ref()
                .and_then(|p| cfg.providers.get(p).and_then(|pc| pc.model.clone()))
        });

    let (provider_id, model) = match raw {
        Some(s) if s.contains('/') => {
            let (p, m) = s.split_once('/').unwrap();
            (p.to_string(), m.to_string())
        }
        Some(s) => {
            let provider = cfg.default_provider.clone().ok_or_else(|| {
                anyhow!("no default_provider configured; pass --model provider/model")
            })?;
            (provider, s)
        }
        None => {
            let provider = cfg.default_provider.clone().ok_or_else(|| {
                anyhow!("no default_provider configured; run `wingman config init`")
            })?;
            let model = cfg
                .providers
                .get(&provider)
                .and_then(|pc| pc.model.clone())
                .ok_or_else(|| anyhow!("no model configured for provider {provider}"))?;
            (provider, model)
        }
    };

    Ok(Selection { provider_id, model })
}

pub fn build_provider(cfg: &Config, provider_id: &str) -> Result<Arc<dyn Provider>> {
    let pc = cfg
        .providers
        .get(provider_id)
        .with_context(|| format!("no [providers.{provider_id}] section in config"))?;

    match provider_id {
        "anthropic" => {
            let key = resolve_api_key(pc.api_key.as_deref(), "ANTHROPIC_API_KEY")?;
            let mut p = AnthropicProvider::new(key)?;
            if let Some(url) = &pc.base_url {
                p = p.with_base_url(url);
            }
            Ok(Arc::new(p))
        }
        "gemini" => {
            let key = resolve_api_key(pc.api_key.as_deref(), "GOOGLE_API_KEY")
                .or_else(|_| resolve_api_key(pc.api_key.as_deref(), "GEMINI_API_KEY"))?;
            let mut p = GeminiProvider::new(key)?;
            if let Some(url) = &pc.base_url {
                p = p.with_base_url(url);
            }
            Ok(Arc::new(p))
        }
        "chatgpt" => {
            let token = resolve_chatgpt_token(pc)?;
            Ok(Arc::new(ChatGptProvider::new(token)?))
        }
        "cohere" => {
            let key = resolve_api_key(pc.api_key.as_deref(), "COHERE_API_KEY")?;
            let mut p = CohereProvider::new(key)?;
            if let Some(url) = &pc.base_url {
                p = p.with_base_url(url);
            }
            Ok(Arc::new(p))
        }
        "watsonx" => {
            // Project id: from [providers.watsonx.project_id] in extras, or
            // `WATSONX_PROJECT_ID` env, or fail with a clear error.
            let project_id = pc
                .extra
                .get("project_id")
                .and_then(|v| v.as_str().map(str::to_string))
                .or_else(|| std::env::var("WATSONX_PROJECT_ID").ok())
                .ok_or_else(|| {
                    anyhow!(
                        "watsonx requires a project_id — set `[providers.watsonx] project_id = \"…\"` \
                         or the `WATSONX_PROJECT_ID` env var"
                    )
                })?;
            // Credential: prefer pre-obtained access token, else API key.
            let credential = if let Ok(t) = std::env::var("WATSONX_ACCESS_TOKEN") {
                if !t.trim().is_empty() {
                    WatsonxCredential::AccessToken(t)
                } else {
                    let key = resolve_api_key(pc.api_key.as_deref(), "WATSONX_API_KEY")?;
                    WatsonxCredential::ApiKey(key)
                }
            } else {
                let key = resolve_api_key(pc.api_key.as_deref(), "WATSONX_API_KEY")?;
                WatsonxCredential::ApiKey(key)
            };
            let mut p = WatsonxProvider::new(credential, project_id)?;
            if let Some(url) = &pc.base_url {
                p = p.with_base_url(url);
            }
            if let Some(iam) = pc.extra.get("iam_url").and_then(|v| v.as_str()) {
                p = p.with_iam_url(iam);
            }
            Ok(Arc::new(p))
        }
        id if openai_variant(id).is_some() => {
            let variant = openai_variant(id).unwrap();
            let key = resolve_optional_api_key(pc.api_key.as_deref(), variant);
            let mut p = OpenAiCompatProvider::new(variant, key)?;
            if let Some(url) = &pc.base_url {
                p = p.with_base_url(url);
            }
            Ok(Arc::new(p))
        }
        other => Err(anyhow!(
            "provider '{other}' is not implemented yet — see README \"Providers\" table for supported ids"
        )),
    }
}

/// Resolve a ChatGPT OAuth access token from the environment or keychain.
///
/// Token lookup order:
///   1. `CHATGPT_ACCESS_TOKEN` env var (useful for CI / headless environments).
///   2. Config value / `keyring:chatgpt` marker.
///   3. OS keychain entry `"chatgpt"` directly.
///
/// If the token has already expired the `ChatGptProvider` will receive a 401
/// and surface a "re-login" error. Use [`refresh_chatgpt_token_if_needed`] in
/// async contexts (e.g., the login runner) to proactively refresh.
fn resolve_chatgpt_token(pc: &wingman_config::ProviderConfig) -> Result<String> {
    if let Ok(t) = std::env::var("CHATGPT_ACCESS_TOKEN") {
        if !t.trim().is_empty() {
            return Ok(t.trim().to_string());
        }
    }
    check_config_value(pc.api_key.as_deref())
        .or_else(|| wingman_config::secrets::load("chatgpt").ok().flatten())
        .ok_or_else(|| {
            anyhow!(
                "no ChatGPT token found — run /login and choose 'ChatGPT (subscription)' \
                 to authenticate via browser"
            )
        })
}

/// Attempt a silent token refresh if the stored access token is expiring.
/// Called from async contexts (agent build, login runner) so it can safely
/// await the HTTP call.
pub async fn refresh_chatgpt_token_if_needed() {
    let access = match wingman_config::secrets::load("chatgpt").ok().flatten() {
        Some(t) => t,
        None => return,
    };
    if !crate::oauth::token_is_expiring(&access, 300) {
        return;
    }
    let refresh = match wingman_config::secrets::load("chatgpt_refresh")
        .ok()
        .flatten()
    {
        Some(r) => r,
        None => return,
    };
    tracing::info!("chatgpt access token expiring; attempting silent refresh");
    match crate::oauth::refresh_chatgpt_token(&refresh).await {
        Ok((new_access, new_refresh)) => {
            let _ = wingman_config::secrets::store("chatgpt", &new_access);
            let _ = wingman_config::secrets::store("chatgpt_refresh", &new_refresh);
            tracing::info!("chatgpt token refreshed successfully");
        }
        Err(e) => {
            tracing::warn!("chatgpt silent refresh failed: {e}");
        }
    }
}

fn openai_variant(id: &str) -> Option<OpenAiVariant> {
    Some(match id {
        "openai" => OpenAiVariant::OpenAI,
        "openrouter" => OpenAiVariant::OpenRouter,
        "lmstudio" | "lm_studio" | "lm-studio" => OpenAiVariant::LmStudio,
        "vllm" => OpenAiVariant::Vllm,
        "litellm" => OpenAiVariant::LiteLlm,
        "ollama" => OpenAiVariant::Ollama,
        "groq" => OpenAiVariant::Groq,
        "together" | "togetherai" | "together_ai" => OpenAiVariant::Together,
        "fireworks" | "fireworks_ai" | "fireworksai" => OpenAiVariant::Fireworks,
        "deepinfra" => OpenAiVariant::DeepInfra,
        "perplexity" | "pplx" => OpenAiVariant::Perplexity,
        "xai" | "grok" => OpenAiVariant::XAI,
        "deepseek" => OpenAiVariant::DeepSeek,
        "mistral" | "mistralai" => OpenAiVariant::Mistral,
        "cerebras" => OpenAiVariant::Cerebras,
        "sambanova" => OpenAiVariant::SambaNova,
        "azure" | "azure_openai" | "azureopenai" => OpenAiVariant::AzureOpenAI,
        "github" | "github_models" | "githubmodels" => OpenAiVariant::GithubModels,
        "llamacpp" | "llama_cpp" | "llama-cpp" => OpenAiVariant::LlamaCpp,
        "tgi" | "hf_tgi" => OpenAiVariant::Tgi,
        "anyscale" => OpenAiVariant::Anyscale,
        "lepton" | "leptonai" => OpenAiVariant::Lepton,
        "replicate" => OpenAiVariant::Replicate,
        "novita" => OpenAiVariant::Novita,
        "hyperbolic" => OpenAiVariant::Hyperbolic,
        "lambda" | "lambdalabs" => OpenAiVariant::Lambda,
        "nebius" => OpenAiVariant::Nebius,
        "hf" | "huggingface" | "hf_inference" => OpenAiVariant::HfInference,
        "glhf" => OpenAiVariant::Glhf,
        "featherless" => OpenAiVariant::Featherless,
        "octoai" => OpenAiVariant::OctoAi,
        "nvidia" | "nim" | "nvidia_nim" => OpenAiVariant::NvidiaNim,
        "avian" => OpenAiVariant::Avian,
        "kluster" => OpenAiVariant::Kluster,
        "inferencenet" | "inference_net" => OpenAiVariant::InferenceNet,
        "snowflake" | "cortex" => OpenAiVariant::Snowflake,
        "databricks" => OpenAiVariant::Databricks,
        "writer" | "palmyra" => OpenAiVariant::Writer,
        "gpt4all" => OpenAiVariant::Gpt4All,
        "jan" | "janai" => OpenAiVariant::Jan,
        "koboldcpp" | "kobold" => OpenAiVariant::KoboldCpp,
        "oobabooga" | "ooba" | "textgenwebui" => OpenAiVariant::Oobabooga,
        "qwen" | "dashscope" | "alibaba" => OpenAiVariant::DashScope,
        "zhipu" | "glm" | "bigmodel" => OpenAiVariant::Zhipu,
        "moonshot" | "kimi" => OpenAiVariant::Moonshot,
        "minimax" => OpenAiVariant::MiniMax,
        "yi" | "lingyiwanwu" | "01ai" => OpenAiVariant::Yi,
        "baichuan" => OpenAiVariant::Baichuan,
        "hunyuan" | "tencent" => OpenAiVariant::Hunyuan,
        "doubao" | "volcengine" | "bytedance" | "ark" => OpenAiVariant::Doubao,
        "siliconflow" | "silicon" => OpenAiVariant::SiliconFlow,
        "cloudflare" | "workersai" | "workers_ai" => OpenAiVariant::Cloudflare,
        "vercel" | "vercel_gateway" => OpenAiVariant::Vercel,
        "aimlapi" | "aiml" => OpenAiVariant::AimlApi,
        "openpipe" => OpenAiVariant::OpenPipe,
        "targon" => OpenAiVariant::Targon,
        "pollinations" => OpenAiVariant::Pollinations,
        "mlx" | "mlx_lm" | "mlxlm" => OpenAiVariant::MlxLm,
        "localai" | "local_ai" => OpenAiVariant::LocalAi,
        "aphrodite" => OpenAiVariant::Aphrodite,
        "mistralrs" | "mistral_rs" => OpenAiVariant::MistralRs,
        "ai21" | "jamba" => OpenAiVariant::Ai21,
        "zai" | "z_ai" | "z-ai" => OpenAiVariant::Zai,
        "friendli" | "friendliai" => OpenAiVariant::Friendli,
        "mancer" => OpenAiVariant::Mancer,
        "reka" => OpenAiVariant::Reka,
        "bedrock" | "aws_bedrock" | "aws-bedrock" => OpenAiVariant::Bedrock,
        "vertex" | "vertex_ai" | "vertexai" | "gcp_vertex" => OpenAiVariant::Vertex,
        _ => return None,
    })
}

fn resolve_optional_api_key(from_config: Option<&str>, variant: OpenAiVariant) -> Option<String> {
    if let Some(key) = check_config_value(from_config) {
        return Some(key);
    }
    let env_name = openai_env_name(variant)?;
    std::env::var(env_name).ok()
}

/// The conventional environment variable that holds the API key for an
/// OpenAI-compatible provider variant. Returns `None` for local providers
/// that don't require a key (LM Studio, Ollama, vLLM, …).
fn openai_env_name(variant: OpenAiVariant) -> Option<&'static str> {
    Some(match variant {
        OpenAiVariant::OpenAI => "OPENAI_API_KEY",
        OpenAiVariant::OpenRouter => "OPENROUTER_API_KEY",
        OpenAiVariant::LiteLlm => "LITELLM_API_KEY",
        OpenAiVariant::Groq => "GROQ_API_KEY",
        OpenAiVariant::Together => "TOGETHER_API_KEY",
        OpenAiVariant::Fireworks => "FIREWORKS_API_KEY",
        OpenAiVariant::DeepInfra => "DEEPINFRA_API_KEY",
        OpenAiVariant::Perplexity => "PERPLEXITY_API_KEY",
        OpenAiVariant::XAI => "XAI_API_KEY",
        OpenAiVariant::DeepSeek => "DEEPSEEK_API_KEY",
        OpenAiVariant::Mistral => "MISTRAL_API_KEY",
        OpenAiVariant::Cerebras => "CEREBRAS_API_KEY",
        OpenAiVariant::SambaNova => "SAMBANOVA_API_KEY",
        OpenAiVariant::AzureOpenAI => "AZURE_OPENAI_API_KEY",
        OpenAiVariant::GithubModels => "GITHUB_TOKEN",
        OpenAiVariant::Anyscale => "ANYSCALE_API_KEY",
        OpenAiVariant::Lepton => "LEPTON_API_KEY",
        OpenAiVariant::Replicate => "REPLICATE_API_TOKEN",
        OpenAiVariant::Novita => "NOVITA_API_KEY",
        OpenAiVariant::Hyperbolic => "HYPERBOLIC_API_KEY",
        OpenAiVariant::Lambda => "LAMBDA_API_KEY",
        OpenAiVariant::Nebius => "NEBIUS_API_KEY",
        OpenAiVariant::HfInference => "HF_TOKEN",
        OpenAiVariant::Glhf => "GLHF_API_KEY",
        OpenAiVariant::Featherless => "FEATHERLESS_API_KEY",
        OpenAiVariant::OctoAi => "OCTOAI_API_KEY",
        OpenAiVariant::NvidiaNim => "NVIDIA_API_KEY",
        OpenAiVariant::Avian => "AVIAN_API_KEY",
        OpenAiVariant::Kluster => "KLUSTER_API_KEY",
        OpenAiVariant::InferenceNet => "INFERENCE_NET_API_KEY",
        OpenAiVariant::Snowflake => "SNOWFLAKE_API_KEY",
        OpenAiVariant::Databricks => "DATABRICKS_TOKEN",
        OpenAiVariant::Writer => "WRITER_API_KEY",
        OpenAiVariant::DashScope => "DASHSCOPE_API_KEY",
        OpenAiVariant::Zhipu => "ZHIPU_API_KEY",
        OpenAiVariant::Moonshot => "MOONSHOT_API_KEY",
        OpenAiVariant::MiniMax => "MINIMAX_API_KEY",
        OpenAiVariant::Yi => "YI_API_KEY",
        OpenAiVariant::Baichuan => "BAICHUAN_API_KEY",
        OpenAiVariant::Hunyuan => "HUNYUAN_API_KEY",
        OpenAiVariant::Doubao => "ARK_API_KEY",
        OpenAiVariant::SiliconFlow => "SILICONFLOW_API_KEY",
        OpenAiVariant::Cloudflare => "CLOUDFLARE_API_TOKEN",
        OpenAiVariant::Vercel => "VERCEL_AI_GATEWAY_KEY",
        OpenAiVariant::AimlApi => "AIMLAPI_KEY",
        OpenAiVariant::OpenPipe => "OPENPIPE_API_KEY",
        OpenAiVariant::Targon => "TARGON_API_KEY",
        OpenAiVariant::Ai21 => "AI21_API_KEY",
        OpenAiVariant::Zai => "ZAI_API_KEY",
        OpenAiVariant::Friendli => "FRIENDLI_TOKEN",
        OpenAiVariant::Mancer => "MANCER_API_KEY",
        OpenAiVariant::Reka => "REKA_API_KEY",
        // Bedrock: long-term API key from the AWS console (Bedrock → API
        // keys). For SigV4 auth against the non-OpenAI surface the user
        // should configure AWS CLI normally; that path will require a
        // dedicated native adapter.
        OpenAiVariant::Bedrock => "AWS_BEARER_TOKEN_BEDROCK",
        // Vertex: short-lived access token. `gcloud auth print-access-token`
        // is the common way to populate it. Service-account JWT signing is
        // out of scope for the OpenAI-compat path.
        OpenAiVariant::Vertex => "GOOGLE_VERTEX_TOKEN",
        OpenAiVariant::LmStudio
        | OpenAiVariant::Vllm
        | OpenAiVariant::Ollama
        | OpenAiVariant::LlamaCpp
        | OpenAiVariant::Tgi
        | OpenAiVariant::Gpt4All
        | OpenAiVariant::Jan
        | OpenAiVariant::KoboldCpp
        | OpenAiVariant::Oobabooga
        | OpenAiVariant::Pollinations
        | OpenAiVariant::MlxLm
        | OpenAiVariant::LocalAi
        | OpenAiVariant::Aphrodite
        | OpenAiVariant::MistralRs => return None,
    })
}

/// The conventional environment variable that holds the API key for a
/// provider id, if one is defined. Used by `wingman login` to auto-read a
/// key when `--api-key` isn't passed. Returns `None` for local providers
/// that need no key and for unknown ids.
pub fn api_key_env_var(provider_id: &str) -> Option<&'static str> {
    match provider_id {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "gemini" => Some("GOOGLE_API_KEY"),
        "cohere" => Some("COHERE_API_KEY"),
        "watsonx" => Some("WATSONX_API_KEY"),
        "chatgpt" => Some("CHATGPT_ACCESS_TOKEN"),
        id => openai_variant(id).and_then(openai_env_name),
    }
}

fn resolve_api_key(from_config: Option<&str>, env_name: &str) -> Result<String> {
    if let Some(key) = check_config_value(from_config) {
        return Ok(key);
    }
    std::env::var(env_name).map_err(|_| {
        anyhow!("missing API key: set [providers.*].api_key in config, store via /login, or set {env_name} in env")
    })
}

/// Inspect a `[providers.*].api_key` config value and turn it into the
/// real key, if any. Recognized forms:
///   - `keyring:<provider_id>`  — look up the OS keyring (Phase B)
///   - non-empty, non-placeholder string — use directly (legacy)
///   - `${ENV_VAR}` placeholder, empty, or missing — return None
fn check_config_value(from_config: Option<&str>) -> Option<String> {
    let s = from_config?;
    let trimmed = s.trim();
    if trimmed.is_empty() || looks_like_placeholder(trimmed) {
        return None;
    }
    if let Some(provider_id) = trimmed.strip_prefix("keyring:") {
        match secrets::load(provider_id) {
            Ok(Some(key)) => return Some(key),
            Ok(None) => {
                tracing::warn!(
                    "config refers to keyring entry for '{provider_id}' but none was found"
                );
                return None;
            }
            Err(e) => {
                tracing::warn!("keyring lookup for '{provider_id}' failed: {e}");
                return None;
            }
        }
    }
    Some(trimmed.to_string())
}

fn looks_like_placeholder(s: &str) -> bool {
    s.trim().starts_with("${") && s.trim().ends_with('}')
}

#[allow(dead_code)] // kept as a public API for callers that don't need learn handles
pub async fn build_registry(cfg: &Config, mode: PermissionMode) -> Result<ToolRegistry> {
    build_registry_with_learn(cfg, mode, None).await
}

pub async fn build_registry_with_learn(
    cfg: &Config,
    mode: PermissionMode,
    learn: Option<Arc<LearnHandles>>,
) -> Result<ToolRegistry> {
    let cwd = std::env::current_dir()?;
    let paths = ProjectPaths::discover(&cwd);
    let ctx = ToolCtx::new_with_config(
        mode,
        cwd,
        paths.root.clone(),
        cfg.tools.shell_denylist.clone(),
        cfg.tools.allow_network,
    );
    // Audit log path: explicit config, else the project's .wingman/audit.log.
    let audit_path = if cfg.audit.enabled {
        Some(
            cfg.audit
                .log_path
                .clone()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| paths.dir.join("audit.log")),
        )
    } else {
        None
    };
    let mut reg = ToolRegistry::new(ctx)
        .with_builtins()
        .with_hooks(cfg.hooks.clone())
        .with_audit(audit_path);
    let indexer = build_indexer(&paths)?;
    if let Some(idx) = indexer.clone() {
        reg = reg.with_semantic_search(idx);
    }

    if let Some(handles) = learn {
        let embedder = pick_embedder();
        // Open the global sessions index (cross-project recall).
        let sess_store = match wingman_learn::session_index::open_global_store(&*embedder) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("disabling cross-project session recall: {e}");
                None
            }
        };
        // Backfill any unindexed sessions in the background so the user
        // can immediately recall recent work.
        if let Some(store) = sess_store.clone() {
            let emb = embedder.clone();
            let root = paths.root.clone();
            tokio::spawn(async move {
                match wingman_learn::session_index::backfill_project_sessions(&root, &store, &*emb)
                    .await
                {
                    Ok(n) if n > 0 => {
                        tracing::info!("backfilled {n} session(s) into sessions.db")
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("session backfill failed: {e}"),
                }
            });
        }
        reg.register(wingman_tools::builtin::SaveMemory::new(
            handles.memory.clone(),
            handles.signals.clone(),
        ));
        reg.register(wingman_tools::builtin::RecallMemory::new(
            handles.memory.clone(),
        ));
        reg.register(wingman_tools::builtin::ForgetMemory::new(
            handles.memory.clone(),
        ));
        reg.register(wingman_tools::builtin::InvokeSkill::new(
            paths.root.clone(),
            handles.stats.clone(),
            handles.signals.clone(),
            handles.hook.config().session_id.clone(),
        ));
        if let Some(store) = sess_store.clone() {
            reg.register(wingman_tools::builtin::RecallSession::new(
                store,
                embedder.clone(),
            ));
        }
        reg.register(wingman_tools::builtin::ReadSession::new(paths.root.clone()));
    }
    // Honor [tools].disabled_tools: a tool the user disabled must not be
    // offered to the model. Applied last so it also removes builtins.
    for name in &cfg.tools.disabled_tools {
        if reg.unregister(name).is_some() {
            tracing::info!(target: "wingman::tools", tool = %name, "disabled via config");
        }
    }
    // MCP servers are connected later via [`McpRegistry::seed`] so the
    // shared `Arc<ToolRegistry>` can be reached from the TUI for runtime
    // add / remove operations.
    Ok(reg)
}

/// Build the project's RAG indexer. Uses fastembed (BGE small) by default
/// and falls back to a deterministic hash embedder if fastembed init fails
/// (e.g. on systems without ONNX runtime libraries).
pub fn build_indexer(paths: &ProjectPaths) -> Result<Option<Arc<Indexer>>> {
    let embedder = pick_embedder();
    let store = match IndexStore::open(&paths.index_db, embedder.id(), embedder.dim()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("disabling RAG: could not open index db: {e}");
            return Ok(None);
        }
    };
    Ok(Some(Arc::new(Indexer::new(
        paths.root.clone(),
        embedder,
        Arc::new(store),
    ))))
}

/// Public wrapper around `pick_embedder` for callers outside this module.
pub fn pick_embedder_pub() -> Arc<dyn Embedder> {
    pick_embedder()
}

fn pick_embedder() -> Arc<dyn Embedder> {
    #[cfg(feature = "embeddings")]
    {
        match wingman_rag::FastembedEmbedder::new(Some(model_cache_dir())) {
            Ok(e) => return Arc::new(e),
            Err(err) => {
                tracing::warn!("fastembed init failed, falling back to hash embedder: {err}");
            }
        }
    }
    Arc::new(HashEmbedder::default())
}

#[cfg(feature = "embeddings")]
fn model_cache_dir() -> std::path::PathBuf {
    wingman_config::global_dir()
        .map(|d| d.join("models"))
        .unwrap_or_else(|_| std::path::PathBuf::from(".wingman/models"))
}

/// Post-edit verification gate that shells out to a check command in the
/// project root (e.g. `cargo check`). Fail-open: if the command can't even
/// spawn (toolchain missing), the gate passes with a note rather than
/// trapping the agent in a retry loop it can't fix.
pub struct ShellTurnGate {
    cmd: String,
    cwd: std::path::PathBuf,
}

impl ShellTurnGate {
    /// Build a gate that runs `cmd` in `cwd`. Used by the E5.5 pilot worker
    /// path, which gates on `[pilot].turn_gate_cmd` rather than
    /// `[verify].turn_gate`.
    pub fn new(cmd: String, cwd: std::path::PathBuf) -> Self {
        Self { cmd, cwd }
    }
}

/// Run `cmd` in `cwd` and render a compact [`GateReport`]. Shared by every
/// shell-backed gate (compile check, affected tests).
async fn run_check_cmd(cmd: &str, cwd: &std::path::Path) -> GateReport {
    let output = if cfg!(windows) {
        tokio::process::Command::new("cmd")
            .args(["/C", cmd])
            .current_dir(cwd)
            .output()
            .await
    } else {
        tokio::process::Command::new("sh")
            .args(["-c", cmd])
            .current_dir(cwd)
            .output()
            .await
    };
    match output {
        Ok(o) => {
            let passed = o.status.success();
            let stderr = String::from_utf8_lossy(&o.stderr);
            let stdout = String::from_utf8_lossy(&o.stdout);
            let body = if stderr.trim().is_empty() {
                stdout
            } else {
                stderr
            };
            // Keep the receipt small: last 40 lines is enough for the
            // model (and the user) to see what broke.
            let lines: Vec<&str> = body.lines().collect();
            let tail = if lines.len() > 40 {
                format!(
                    "… ({} lines omitted)\n{}",
                    lines.len() - 40,
                    lines[lines.len() - 40..].join("\n")
                )
            } else {
                lines.join("\n")
            };
            let mark = if passed { "✓ passed" } else { "✗ failed" };
            GateReport {
                passed,
                summary: format!("$ {cmd}\n{mark}\n{tail}").trim_end().to_string(),
            }
        }
        Err(e) => GateReport {
            passed: true,
            summary: format!("$ {cmd}\n⚠ gate skipped (could not run: {e})"),
        },
    }
}

#[async_trait::async_trait]
impl TurnGate for ShellTurnGate {
    fn label(&self) -> String {
        self.cmd.clone()
    }

    async fn check(&self) -> GateReport {
        run_check_cmd(&self.cmd, &self.cwd).await
    }
}

/// Runs the tests of the crates changed this turn (via `git`), not the whole
/// suite. Discovers changed crates at check-time so it tracks whatever the
/// agent edited. A no-op (passes) when nothing relevant changed.
///
/// ponytail: crate-level granularity, not symbol→test mapping. Upgrade path is
/// mapping edited symbols to the specific tests that reference them.
pub struct AffectedTestsGate {
    root: std::path::PathBuf,
}

#[async_trait::async_trait]
impl TurnGate for AffectedTestsGate {
    fn label(&self) -> String {
        "affected tests".into()
    }

    async fn check(&self) -> GateReport {
        let crates = changed_rust_crates(&self.root);
        if crates.is_empty() {
            return GateReport {
                passed: true,
                summary: "affected tests: none (no changed Rust crates)".into(),
            };
        }
        let mut cmd = String::from("cargo test --quiet");
        for c in &crates {
            cmd.push_str(" -p ");
            cmd.push_str(c);
        }
        let mut report = run_check_cmd(&cmd, &self.root).await;

        // Surface the symbols actually edited this turn (tree-sitter-mapped from
        // the diff hunks) in the receipt. We deliberately do NOT narrow the
        // `cargo test` invocation to these symbol names: a name filter that
        // matches no test would run zero tests and pass — a false green that's
        // worse than running the whole changed crate. Safe symbol-level
        // narrowing needs resolved references from `test fn -> edited symbol`
        // (LSP), which is the follow-on; until then crate-level is the floor.
        let symbols = edited_symbol_names(&self.root);
        if !symbols.is_empty() {
            let shown: Vec<&str> = symbols.iter().take(12).map(String::as_str).collect();
            let more = symbols.len().saturating_sub(shown.len());
            report.summary = format!(
                "edited symbols: {}{}\n{}",
                shown.join(", "),
                if more > 0 {
                    format!(" (+{more})")
                } else {
                    String::new()
                },
                report.summary
            );
        }
        report
    }
}

/// Repo-relative changed file -> 1-based line numbers touched, from
/// `git diff --unified=0` hunk headers. Empty on non-git repos.
fn changed_lines_by_file(
    root: &std::path::Path,
) -> std::collections::HashMap<String, std::collections::BTreeSet<u32>> {
    let mut map: std::collections::HashMap<String, std::collections::BTreeSet<u32>> =
        std::collections::HashMap::new();
    let out = std::process::Command::new("git")
        .args(["diff", "--unified=0"])
        .current_dir(root)
        .output();
    let Ok(out) = out else {
        return map;
    };
    if !out.status.success() {
        return map;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            current = Some(rest.trim().to_string());
        } else if line.starts_with("@@") {
            // `@@ -a,b +c,d @@` — take the `+c,d` (new-side) span.
            if let Some(plus) = line.split('+').nth(1) {
                let spec = plus.split([' ', '@']).next().unwrap_or("");
                let mut it = spec.split(',');
                let start: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                let count: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(1);
                if start > 0 {
                    if let Some(f) = &current {
                        let set = map.entry(f.clone()).or_default();
                        for l in start..start + count.max(1) {
                            set.insert(l);
                        }
                    }
                }
            }
        }
    }
    map
}

/// Names of symbols whose definition span overlaps a changed line this turn,
/// via tree-sitter over the changed files. Used to make the affected-tests
/// receipt informative (which symbols changed). Empty without tree-sitter.
#[cfg(feature = "treesitter")]
fn edited_symbol_names(root: &std::path::Path) -> Vec<String> {
    let changed = changed_lines_by_file(root);
    let mut names = std::collections::BTreeSet::new();
    for (path, lines) in changed {
        let abs = root.join(&path);
        let Some(lang) = wingman_ts::Language::from_path(&abs) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&abs) else {
            continue;
        };
        for sym in wingman_ts::extract_symbols(lang, &text) {
            if lines.range(sym.start_line..=sym.end_line).next().is_some() {
                names.insert(sym.name);
            }
        }
    }
    names.into_iter().collect()
}

#[cfg(not(feature = "treesitter"))]
fn edited_symbol_names(_root: &std::path::Path) -> Vec<String> {
    Vec::new()
}

/// Headless-browser visual verification: load a URL, screenshot it, and fail
/// if it differs from a committed baseline by more than a threshold — proving
/// a UI change renders. Fail-open when no browser is available (the `browser`
/// feature is off, or no Chrome binary), so it never traps the agent.
pub struct BrowserGate {
    root: std::path::PathBuf,
    cfg: wingman_config::BrowserVerifyConfig,
}

#[async_trait::async_trait]
impl TurnGate for BrowserGate {
    fn label(&self) -> String {
        "browser".into()
    }

    async fn check(&self) -> GateReport {
        let url = self.cfg.url.clone();
        let timeout = std::time::Duration::from_secs(30);
        let capture =
            tokio::task::spawn_blocking(move || wingman_browser::capture(&url, timeout)).await;
        let capture = match capture {
            Ok(Ok(c)) => c,
            // Fail-open: no browser / feature off / navigation failure.
            Ok(Err(e)) => {
                return GateReport {
                    passed: true,
                    summary: format!("browser: ⚠ skipped ({e})"),
                }
            }
            Err(_) => {
                return GateReport {
                    passed: true,
                    summary: "browser: ⚠ skipped (capture task failed)".into(),
                }
            }
        };

        let Some(rel) = &self.cfg.baseline else {
            return GateReport {
                passed: true,
                summary: "browser: captured (no baseline configured to compare)".into(),
            };
        };
        let baseline = self.root.join(rel);
        if !baseline.exists() {
            if let Some(parent) = baseline.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&baseline, &capture.screenshot_png);
            return GateReport {
                passed: true,
                summary: format!("browser: baseline captured → {}", baseline.display()),
            };
        }
        let base = std::fs::read(&baseline).unwrap_or_default();
        match wingman_browser::diff_ratio(&base, &capture.screenshot_png, self.cfg.tolerance) {
            Ok(ratio) if ratio <= self.cfg.threshold => GateReport {
                passed: true,
                summary: format!(
                    "browser: ✓ {:.2}% pixels changed (≤ {:.2}% threshold)",
                    ratio * 100.0,
                    self.cfg.threshold * 100.0
                ),
            },
            Ok(ratio) => GateReport {
                passed: false,
                summary: format!(
                    "browser: ✗ {:.2}% pixels changed (> {:.2}% threshold) at {} — update the baseline if intended",
                    ratio * 100.0,
                    self.cfg.threshold * 100.0,
                    self.cfg.url
                ),
            },
            Err(e) => GateReport {
                passed: false,
                summary: format!("browser: ✗ screenshot diff failed: {e}"),
            },
        }
    }
}

/// Runs several gates in sequence, short-circuiting on the first failure so a
/// broken compile doesn't waste time running tests. Passes only if all pass.
pub struct CompositeGate {
    gates: Vec<Arc<dyn TurnGate>>,
}

#[async_trait::async_trait]
impl TurnGate for CompositeGate {
    fn label(&self) -> String {
        self.gates
            .iter()
            .map(|g| g.label())
            .collect::<Vec<_>>()
            .join(" + ")
    }

    async fn check(&self) -> GateReport {
        let mut summaries = Vec::new();
        for g in &self.gates {
            let r = g.check().await;
            summaries.push(r.summary);
            if !r.passed {
                return GateReport {
                    passed: false,
                    summary: summaries.join("\n\n"),
                };
            }
        }
        GateReport {
            passed: true,
            summary: summaries.join("\n\n"),
        }
    }
}

/// Changed Rust crates (by package name) in the working tree, via
/// `git status --porcelain`. Maps each changed `.rs` file up to its nearest
/// `Cargo.toml` and reads the package name. Empty on non-git repos or when
/// nothing Rust changed.
fn changed_rust_crates(root: &std::path::Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain", "-z"])
        .current_dir(root)
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let mut crates: Vec<String> = Vec::new();
    // `-z` gives NUL-separated `XY <path>` entries where `XY` is exactly two
    // status columns followed by a space — so the path starts at byte 3. Do
    // NOT trim: a leading space in `XY` (e.g. " M") is significant alignment.
    // Renames emit a second NUL field (the old path) with no status prefix;
    // it won't map to a real file so it's harmlessly ignored.
    for entry in String::from_utf8_lossy(&out.stdout).split('\0') {
        if entry.len() < 4 {
            continue;
        }
        let path = &entry[3..]; // strip "XY " status prefix
        if !path.ends_with(".rs") {
            continue;
        }
        if let Some(name) = crate_name_for(root, std::path::Path::new(path)) {
            if !crates.contains(&name) {
                crates.push(name);
            }
        }
    }
    crates.sort();
    crates
}

/// Changed files in the working tree (absolute paths) whose extension maps to
/// a language server we can drive. Shares the `git status --porcelain -z`
/// parsing of [`changed_rust_crates`]. Empty on non-git repos.
fn changed_lsp_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain", "-z"])
        .current_dir(root)
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let mut files = Vec::new();
    for entry in String::from_utf8_lossy(&out.stdout).split('\0') {
        if entry.len() < 4 {
            continue;
        }
        let path = &entry[3..]; // strip "XY " status prefix
        let abs = root.join(path);
        // Deletions won't exist on disk; skip so we don't ask the server about
        // a vanished file.
        if wingman_lsp::Lang::from_path(&abs).is_some() && abs.exists() {
            files.push(abs);
        }
    }
    files
}

/// Post-edit gate that folds the language server's diagnostics for the files
/// changed this turn into verification. Fail-open: no server installed, no
/// changed files, or a server error all pass (with a note) rather than trapping
/// the agent. Only genuine *errors* (severity 1) fail the gate; warnings are
/// reported but don't block.
pub struct LspDiagnosticsGate {
    root: std::path::PathBuf,
}

#[async_trait::async_trait]
impl TurnGate for LspDiagnosticsGate {
    fn label(&self) -> String {
        "lsp diagnostics".into()
    }

    async fn check(&self) -> GateReport {
        let files = changed_lsp_files(&self.root);
        if files.is_empty() {
            return GateReport {
                passed: true,
                summary: "lsp diagnostics: none (no changed files a server handles)".into(),
            };
        }
        let manager = wingman_lsp::manager_for(&self.root).await;
        let mut error_lines: Vec<String> = Vec::new();
        let mut checked = 0usize;
        let mut no_server = 0usize;
        for file in &files {
            let client = match manager.client_for_path(file).await {
                Ok(c) => c,
                Err(_) => {
                    no_server += 1;
                    continue;
                }
            };
            let diags = client
                .diagnostics(file, std::time::Duration::from_secs(15))
                .await
                .unwrap_or_default();
            checked += 1;
            let rel = file
                .strip_prefix(&self.root)
                .unwrap_or(file)
                .to_string_lossy()
                .replace('\\', "/");
            for d in diags.iter().filter(|d| d.is_error()) {
                error_lines.push(format!(
                    "{rel}:{}:{} error: {}",
                    d.line + 1,
                    d.character + 1,
                    d.message.replace('\n', " ")
                ));
            }
        }

        if checked == 0 {
            return GateReport {
                passed: true,
                summary: format!(
                    "lsp diagnostics: ⚠ skipped (no language server on PATH for {no_server} changed file(s))"
                ),
            };
        }
        if error_lines.is_empty() {
            GateReport {
                passed: true,
                summary: format!("lsp diagnostics: ✓ no errors in {checked} changed file(s)"),
            }
        } else {
            GateReport {
                passed: false,
                summary: format!(
                    "lsp diagnostics: ✗ {} error(s) in changed files\n{}",
                    error_lines.len(),
                    error_lines.join("\n")
                ),
            }
        }
    }
}

/// Walk up from a repo-relative file path to the nearest `Cargo.toml` and read
/// its `[package] name`. Returns None if no manifest or no name found.
fn crate_name_for(root: &std::path::Path, rel_file: &std::path::Path) -> Option<String> {
    let mut dir = root.join(rel_file);
    dir.pop(); // drop filename
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists() {
            if let Some(name) = package_name(&manifest) {
                return Some(name);
            }
        }
        if !dir.starts_with(root) || !dir.pop() {
            return None;
        }
    }
}

/// Minimal `[package] name = "…"` extractor. ponytail: line scan, not a full
/// TOML parse — good enough for standard manifests; workspace-only manifests
/// (no `[package]`) correctly yield None.
fn package_name(manifest: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package {
            if let Some(rest) = line.strip_prefix("name") {
                let rest = rest.trim_start().strip_prefix('=')?.trim();
                return Some(rest.trim_matches(['"', '\'']).to_string());
            }
        }
    }
    None
}

/// Pick a check command from the project type. Conservative: only commands
/// whose project marker file exists, cheapest reasonable check per ecosystem.
pub fn detect_turn_gate_cmd(root: &std::path::Path) -> Option<String> {
    if root.join("Cargo.toml").exists() {
        Some("cargo check --workspace --quiet".into())
    } else if root.join("tsconfig.json").exists() {
        Some("npx tsc --noEmit".into())
    } else if root.join("go.mod").exists() {
        Some("go build ./...".into())
    } else if root.join("pyproject.toml").exists() || root.join("setup.py").exists() {
        Some("python -m compileall -q .".into())
    } else {
        None
    }
}

/// Resolve `[verify].turn_gate` ("auto" / "off" / explicit command) into a
/// gate instance, or `None` when gating is off or undetectable.
pub fn build_turn_gate(cfg: &Config, root: &std::path::Path) -> Option<Arc<dyn TurnGate>> {
    let cmd = match cfg.verify.turn_gate.trim() {
        "off" | "" => return None,
        "auto" => detect_turn_gate_cmd(root)?,
        explicit => explicit.to_string(),
    };
    let compile: Arc<dyn TurnGate> = Arc::new(ShellTurnGate {
        cmd,
        cwd: root.to_path_buf(),
    });

    // Compose optional gates after the compile check, short-circuiting on the
    // first failure (compile → affected tests → lsp diagnostics).
    let mut gates: Vec<Arc<dyn TurnGate>> = vec![compile];
    if cfg.verify.affected_tests && root.join("Cargo.toml").exists() {
        gates.push(Arc::new(AffectedTestsGate {
            root: root.to_path_buf(),
        }));
    }
    if cfg.verify.lsp_diagnostics {
        gates.push(Arc::new(LspDiagnosticsGate {
            root: root.to_path_buf(),
        }));
    }
    if !cfg.verify.browser.url.trim().is_empty() {
        gates.push(Arc::new(BrowserGate {
            root: root.to_path_buf(),
            cfg: cfg.verify.browser.clone(),
        }));
    }

    if gates.len() == 1 {
        Some(gates.into_iter().next().unwrap())
    } else {
        Some(Arc::new(CompositeGate { gates }))
    }
}

/// On failure, walks `cfg.router.fallback_models`
/// in order until one succeeds. The selection that actually built is
/// printed to stderr so the user knows what's serving the request.
pub async fn build_agent_with_fallback(
    cfg: &Config,
    selection: &Selection,
    mode: PermissionMode,
) -> Result<AgentLoop> {
    let (agent, _registry) = build_agent_registry_with_fallback(cfg, selection, mode).await?;
    Ok(agent)
}

/// Like `build_agent_with_fallback` but also returns the shared tool registry
/// so the caller can seed MCP tools into it (see [`seed_mcp`]). Headless and
/// batch use this so `mcp__*` tools are available there too, not just the TUI.
pub async fn build_agent_registry_with_fallback(
    cfg: &Config,
    selection: &Selection,
    mode: PermissionMode,
) -> Result<(AgentLoop, Arc<ToolRegistry>)> {
    match build_agent_and_registry(cfg, selection, mode).await {
        Ok(pair) => Ok(pair),
        Err(primary_err) => {
            for raw in &cfg.router.fallback_models {
                let Some((p, m)) = raw.split_once('/') else {
                    tracing::warn!("skipping fallback '{raw}': expected provider/model");
                    continue;
                };
                let sel = Selection {
                    provider_id: p.to_string(),
                    model: m.to_string(),
                };
                match build_agent_and_registry(cfg, &sel, mode).await {
                    Ok(pair) => {
                        eprintln!(
                            "wingman: primary failed ({primary_err}); falling back to {}/{}",
                            sel.provider_id, sel.model
                        );
                        return Ok(pair);
                    }
                    Err(e) => {
                        tracing::warn!("fallback {raw} failed: {e}");
                    }
                }
            }
            Err(primary_err)
        }
    }
}

/// Spawn the configured MCP servers and register their tools into `registry`
/// (the same `Arc` the agent dispatches through). Returns the registry handle,
/// which the caller must keep alive for the duration of the run — dropping it
/// tears down the MCP server subprocesses. Best-effort: a server that fails to
/// start is logged by `seed` and simply contributes no tools.
pub async fn seed_mcp(
    cfg: &Config,
    registry: Arc<ToolRegistry>,
) -> Arc<crate::mcp_registry::McpRegistry> {
    let mcp = Arc::new(crate::mcp_registry::McpRegistry::new(registry));
    mcp.seed(&cfg.mcp).await;
    mcp
}

/// Variant that also returns the shared `Arc<ToolRegistry>` so callers can
/// register/unregister tools at runtime (used by the MCP registry).
pub async fn build_agent_and_registry(
    cfg: &Config,
    selection: &Selection,
    mode: PermissionMode,
) -> Result<(AgentLoop, Arc<ToolRegistry>)> {
    let (agent, registry, _learn) = build_agent_registry_learn(cfg, selection, mode).await?;
    Ok((agent, registry))
}

/// Full variant that also returns the [`LearnHandles`] so callers (TUI / CLI)
/// can poke the memory store, stats db, or trigger session indexing.
pub async fn build_agent_registry_learn(
    cfg: &Config,
    selection: &Selection,
    mode: PermissionMode,
) -> Result<(AgentLoop, Arc<ToolRegistry>, Option<Arc<LearnHandles>>)> {
    // Proactively refresh the ChatGPT token before building the provider.
    if selection.provider_id == "chatgpt" {
        refresh_chatgpt_token_if_needed().await;
    }
    let provider = build_provider(cfg, &selection.provider_id)?;

    let cwd = std::env::current_dir().unwrap_or_default();
    let paths = ProjectPaths::discover(&cwd);

    // Build the learn hook first so we can hand its memory/stats handles to
    // the tool registry (some tools need to read/write them).
    let session_id = format!("session-{}", chrono_like_now());
    let learn_cfg = LearnConfig::new(paths.root.clone(), session_id);
    // Give the learn hook the project index so it can inject relevant code
    // locations per turn (search escalation). Cheap: opens the store, no reindex.
    let learn_indexer = build_indexer(&paths).ok().flatten();
    let learn = match LearnHandles::build_with_indexer(learn_cfg, learn_indexer) {
        Ok(h) => Some(Arc::new(h)),
        Err(e) => {
            tracing::warn!("disabling learning loop: {e}");
            None
        }
    };

    let registry = Arc::new(build_registry_with_learn(cfg, mode, learn.clone()).await?);

    // Register the `spawn_subagent` tool against this registry. The runner
    // closure builds a fresh inner agent on each call — crucially WITHOUT
    // `spawn_subagent` registered on it, so recursion is bounded to depth 1.
    {
        let cfg_for_runner = cfg.clone();
        let mode_for_runner = mode;
        let runner: wingman_tools::builtin::SubagentRunner =
            Arc::new(move |spec: wingman_tools::builtin::SubagentSpec| {
                let cfg = cfg_for_runner.clone();
                let mode = mode_for_runner;
                Box::pin(async move {
                    // Model resolution: explicit override > task-class routing
                    // ([router.classes], e.g. search/summarize → fast_model) >
                    // the session's default selection.
                    let sel = if spec.model.contains('/') {
                        let (p, m) = spec.model.split_once('/').unwrap();
                        Selection {
                            provider_id: p.to_string(),
                            model: m.to_string(),
                        }
                    } else if let Some((p, m)) = cfg
                        .router
                        .resolve_class(&spec.task_class)
                        .as_deref()
                        .and_then(|s| s.split_once('/'))
                        .map(|(p, m)| (p.to_string(), m.to_string()))
                    {
                        tracing::info!("routing subagent (class '{}') to {p}/{m}", spec.task_class);
                        Selection {
                            provider_id: p,
                            model: m,
                        }
                    } else {
                        resolve_selection(&cfg, None).map_err(|e| e.to_string())?
                    };
                    let provider =
                        build_provider(&cfg, &sel.provider_id).map_err(|e| e.to_string())?;
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let paths = ProjectPaths::discover(&cwd);
                    let ctx = ToolCtx::new_with_config(
                        mode,
                        cwd,
                        paths.root.clone(),
                        cfg.tools.shell_denylist.clone(),
                        cfg.tools.allow_network,
                    );
                    let mut inner_reg = ToolRegistry::new(ctx)
                        .with_builtins()
                        .with_hooks(cfg.hooks.clone());
                    if let Ok(Some(idx)) = build_indexer(&paths) {
                        inner_reg = inner_reg.with_semantic_search(idx);
                    }
                    let inner_reg = Arc::new(inner_reg);
                    let agent_cfg = AgentConfig {
                        model: sel.model.clone(),
                        system: Some(format!(
                            "You are an isolated subagent invoked by a parent. \
                             Focus narrowly on the task; respond with only the \
                             final answer (no preamble). Description: {}",
                            spec.description
                        )),
                        ..Default::default()
                    };
                    let mut agent = AgentLoop::new(provider, inner_reg, agent_cfg);
                    let mut stream = agent.run(spec.task);
                    let mut out = String::new();
                    use futures::StreamExt;
                    while let Some(ev) = stream.next().await {
                        match ev {
                            wingman_core::AgentEvent::TextDelta { text } => out.push_str(&text),
                            wingman_core::AgentEvent::Error { message } => return Err(message),
                            wingman_core::AgentEvent::Stop { .. } => break,
                            _ => {}
                        }
                    }
                    Ok(out)
                })
            });
        registry.register_arc(Arc::new(wingman_tools::builtin::SpawnSubagent::new(runner)));
    }

    // Compose the system prompt: base + memory index + skills catalog.
    let memory_store = learn
        .as_ref()
        .map(|l| l.memory.clone())
        .unwrap_or_else(|| Arc::new(MemoryStore::new(paths.root.clone())));
    let skills = wingman_skills::load_all(&paths.root);
    let system = build_system_prompt_full(mode, &memory_store, &skills);

    // Honor [tokens].prompt_cache: when off, drop the cache breakpoints and
    // the rolling conversation-cache marker so no provider-side prompt caching
    // is requested. (Previously this knob was ignored — caching was always on.)
    let (cache_breakpoints, cache_conversation) = if cfg.tokens.prompt_cache {
        (
            vec![
                wingman_core::CacheBreakpoint::AfterSystem,
                wingman_core::CacheBreakpoint::AfterTools,
            ],
            true,
        )
    } else {
        (Vec::new(), false)
    };
    let agent_cfg = AgentConfig {
        model: selection.model.clone(),
        system: Some(system),
        tool_output_budget: ToolOutputBudget::new(cfg.effective_tool_output_max_lines()),
        compactor: Compactor {
            trigger_tokens: cfg.tokens.compact_at_tokens,
            ..Default::default()
        },
        cache_breakpoints,
        cache_conversation,
        learning: learn
            .as_ref()
            .map(|l| l.hook.clone() as Arc<dyn wingman_core::LearningHook>),
        gate: build_turn_gate(cfg, &paths.root),
        gate_max_retries: cfg.verify.max_retries as usize,
        ..Default::default()
    };
    let agent = AgentLoop::new(provider, registry.clone(), agent_cfg);
    Ok((agent, registry, learn))
}

fn chrono_like_now() -> String {
    // Avoid pulling chrono just for this — use the system clock.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{ts}")
}

#[allow(dead_code)] // kept as a back-compat helper for headless / json mode
pub fn build_system_prompt(mode: PermissionMode) -> String {
    // Kept for backwards compatibility (e.g. headless / json mode that
    // doesn't load memory). Real chat mode uses `build_system_prompt_full`.
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    base_prompt(mode, &cwd)
}

/// Compose the full system prompt, including memory index + available skills
/// + recall/save hints. Memory bodies are NOT included inline — the agent
///   uses `recall_memory` to fetch them on demand.
pub fn build_system_prompt_full(
    mode: PermissionMode,
    memory: &MemoryStore,
    skills: &[Skill],
) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    let mut s = base_prompt(mode, &cwd);

    let memories = memory.load_all();
    if let Some(block) = wingman_learn::memory::render_prompt_block(&memories) {
        s.push('\n');
        s.push_str(&block);
    }

    if !skills.is_empty() {
        s.push('\n');
        s.push_str("# Available skills\n");
        for sk in skills {
            s.push_str(&format!(
                "- {} — {} [{}]\n",
                sk.name,
                truncate_line(&sk.description, 140),
                sk.source.label(),
            ));
        }
        s.push_str(
            "(Call the `invoke_skill` tool with a name to inject the full skill body for the next turn.)\n",
        );
    }

    s
}

fn truncate_line(s: &str, max: usize) -> String {
    let one = s.replace(['\n', '\r'], " ");
    if one.chars().count() <= max {
        return one;
    }
    let mut out: String = one.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn base_prompt(mode: PermissionMode, cwd: &str) -> String {
    format!(
        "You are wingman, a self-improving terminal coding agent. You help the user inspect, \
         edit, and run code from the command line.\n\
         \n\
         Available tools include: semantic_search, read_file, write_file, edit_file, run_shell, \
         list_dir, glob, grep, save_memory, recall_memory, invoke_skill, recall_session, read_session.\n\
         \n\
         Style rules:\n\
         - For \"where is X\" or \"how does Y work\" questions, call `semantic_search` first \
         to find the relevant chunks, then `read_file` the specific line range you need. \
         Avoid reading whole files when a targeted range will do.\n\
         - Use `grep` for exact-string lookups and `glob` for filename patterns; \
         use `semantic_search` for conceptual / fuzzy queries.\n\
         - Edit with `edit_file` and include enough surrounding context that `old_string` is unique.\n\
         - Verify your edits when reasonable (compile, run a test, re-read the diff).\n\
         - Be concise. Don't restate what the diff already shows.\n\
         \n\
         Self-improvement:\n\
         - When the user says \"remember\", \"save this\", \"from now on\", or expresses a stable \
         preference, call `save_memory` so the next session has it.\n\
         - When the user asks \"have we discussed this before\" or \"how did we fix X last time\", \
         call `recall_session` first.\n\
         - When a skill from the catalog below clearly applies, call `invoke_skill` to load it; \
         performing the task well in the resulting turn improves its usage stats.\n\
         \n\
         Environment:\n\
         - Working directory: {cwd}\n\
         - Permission mode: {mode}\n"
    )
}

#[cfg(test)]
mod affected_tests_tests {
    use super::*;
    use wingman_core::{GateReport, TurnGate};

    #[test]
    fn package_name_reads_package_section_only() {
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().join("Cargo.toml");
        std::fs::write(
            &m,
            "[workspace]\nmembers = [\"a\"]\n\n[package]\nname = \"my-crate\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert_eq!(package_name(&m).as_deref(), Some("my-crate"));

        // Workspace-only manifest (no [package]) → None.
        let m2 = dir.path().join("Cargo2.toml");
        std::fs::write(&m2, "[workspace]\nmembers = [\"a\"]\n").unwrap();
        assert_eq!(package_name(&m2), None);
    }

    #[test]
    fn crate_name_walks_up_to_manifest() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        std::fs::create_dir_all(root.join("crates/foo/src")).unwrap();
        std::fs::write(
            root.join("crates/foo/Cargo.toml"),
            "[package]\nname = \"foo\"\n",
        )
        .unwrap();
        let name = crate_name_for(root, std::path::Path::new("crates/foo/src/lib.rs"));
        assert_eq!(name.as_deref(), Some("foo"));
    }

    struct FixedGate(bool, &'static str);
    #[async_trait::async_trait]
    impl TurnGate for FixedGate {
        fn label(&self) -> String {
            self.1.into()
        }
        async fn check(&self) -> GateReport {
            GateReport {
                passed: self.0,
                summary: self.1.into(),
            }
        }
    }

    #[tokio::test]
    async fn composite_short_circuits_on_first_failure() {
        // Second gate must not appear in the summary when the first fails.
        let g = CompositeGate {
            gates: vec![
                Arc::new(FixedGate(false, "compile-failed")),
                Arc::new(FixedGate(true, "tests-ran")),
            ],
        };
        let r = g.check().await;
        assert!(!r.passed);
        assert!(r.summary.contains("compile-failed"));
        assert!(
            !r.summary.contains("tests-ran"),
            "should have short-circuited"
        );
    }

    #[tokio::test]
    async fn composite_passes_when_all_pass() {
        let g = CompositeGate {
            gates: vec![
                Arc::new(FixedGate(true, "a")),
                Arc::new(FixedGate(true, "b")),
            ],
        };
        let r = g.check().await;
        assert!(r.passed);
        assert!(r.summary.contains("a") && r.summary.contains("b"));
    }

    #[test]
    fn changed_rust_crates_parses_porcelain() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::create_dir_all(root.join("crates/foo/src")).unwrap();
        std::fs::write(
            root.join("crates/foo/Cargo.toml"),
            "[package]\nname=\"foo\"\n",
        )
        .unwrap();
        std::fs::write(root.join("crates/foo/src/lib.rs"), "// v1\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        // Modify the file — it must show up as a changed crate.
        std::fs::write(root.join("crates/foo/src/lib.rs"), "// v2 changed\n").unwrap();
        assert_eq!(changed_rust_crates(root), vec!["foo".to_string()]);
        // A changed non-rs file contributes no crate.
        std::fs::write(root.join("README.md"), "x").unwrap();
        assert_eq!(changed_rust_crates(root), vec!["foo".to_string()]);
    }
}
