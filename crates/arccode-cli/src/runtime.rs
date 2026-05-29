//! Wires config into a concrete `Provider` + `ToolRegistry` + `AgentLoop`.
//!
//! Keeps the per-provider plumbing in one place so command handlers
//! (headless --print, --json, future TUI) can just ask `Runtime::build(...)`.

use anyhow::{anyhow, Context, Result};
use arccode_config::{secrets, Config, PermissionMode, ProjectPaths};
use arccode_core::{AgentConfig, AgentLoop, Compactor, Provider, ToolOutputBudget};
use arccode_learn::{
    hooks::{LearnConfig, LearnHandles},
    memory::MemoryStore,
};
use arccode_providers::{
    AnthropicProvider, ChatGptProvider, CohereProvider, GeminiProvider, OpenAiCompatProvider,
    OpenAiVariant,
};
use arccode_rag::{Embedder, HashEmbedder, IndexStore, Indexer};
use arccode_skills::Skill;
use arccode_tools::{ToolCtx, ToolRegistry};
use std::sync::Arc;

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
                anyhow!("no default_provider configured; run `arccode config init`")
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
fn resolve_chatgpt_token(pc: &arccode_config::ProviderConfig) -> Result<String> {
    if let Ok(t) = std::env::var("CHATGPT_ACCESS_TOKEN") {
        if !t.trim().is_empty() {
            return Ok(t.trim().to_string());
        }
    }
    check_config_value(pc.api_key.as_deref())
        .or_else(|| arccode_config::secrets::load("chatgpt").ok().flatten())
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
    let access = match arccode_config::secrets::load("chatgpt").ok().flatten() {
        Some(t) => t,
        None => return,
    };
    if !crate::oauth::token_is_expiring(&access, 300) {
        return;
    }
    let refresh = match arccode_config::secrets::load("chatgpt_refresh").ok().flatten() {
        Some(r) => r,
        None => return,
    };
    tracing::info!("chatgpt access token expiring; attempting silent refresh");
    match crate::oauth::refresh_chatgpt_token(&refresh).await {
        Ok((new_access, new_refresh)) => {
            let _ = arccode_config::secrets::store("chatgpt", &new_access);
            let _ = arccode_config::secrets::store("chatgpt_refresh", &new_refresh);
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
        _ => return None,
    })
}

fn resolve_optional_api_key(from_config: Option<&str>, variant: OpenAiVariant) -> Option<String> {
    if let Some(key) = check_config_value(from_config) {
        return Some(key);
    }
    let env_name = match variant {
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
    };
    std::env::var(env_name).ok()
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
    );
    let mut reg = ToolRegistry::new(ctx)
        .with_builtins()
        .with_hooks(cfg.hooks.clone());
    let indexer = build_indexer(&paths)?;
    if let Some(idx) = indexer.clone() {
        reg = reg.with_semantic_search(idx);
    }

    if let Some(handles) = learn {
        let embedder = pick_embedder();
        // Open the global sessions index (cross-project recall).
        let sess_store = match arccode_learn::session_index::open_global_store(&*embedder) {
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
                match arccode_learn::session_index::backfill_project_sessions(
                    &root, &store, &*emb,
                )
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
        reg.register(arccode_tools::builtin::SaveMemory::new(
            handles.memory.clone(),
            handles.signals.clone(),
        ));
        reg.register(arccode_tools::builtin::RecallMemory::new(
            handles.memory.clone(),
        ));
        reg.register(arccode_tools::builtin::ForgetMemory::new(
            handles.memory.clone(),
        ));
        reg.register(arccode_tools::builtin::InvokeSkill::new(
            paths.root.clone(),
            handles.stats.clone(),
            handles.signals.clone(),
            handles.hook.config().session_id.clone(),
        ));
        if let Some(store) = sess_store.clone() {
            reg.register(arccode_tools::builtin::RecallSession::new(
                store,
                embedder.clone(),
            ));
        }
        reg.register(arccode_tools::builtin::ReadSession::new(paths.root.clone()));
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
        match arccode_rag::FastembedEmbedder::new(Some(model_cache_dir())) {
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
    arccode_config::global_dir()
        .map(|d| d.join("models"))
        .unwrap_or_else(|_| std::path::PathBuf::from(".arccode/models"))
}

pub async fn build_agent(
    cfg: &Config,
    selection: &Selection,
    mode: PermissionMode,
) -> Result<AgentLoop> {
    let (agent, _registry) = build_agent_and_registry(cfg, selection, mode).await?;
    Ok(agent)
}

/// Like `build_agent` but, on failure, walks `cfg.router.fallback_models`
/// in order until one succeeds. The selection that actually built is
/// printed to stderr so the user knows what's serving the request.
pub async fn build_agent_with_fallback(
    cfg: &Config,
    selection: &Selection,
    mode: PermissionMode,
) -> Result<AgentLoop> {
    match build_agent(cfg, selection, mode).await {
        Ok(a) => Ok(a),
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
                match build_agent(cfg, &sel, mode).await {
                    Ok(a) => {
                        eprintln!(
                            "arccode: primary failed ({primary_err}); falling back to {}/{}",
                            sel.provider_id, sel.model
                        );
                        return Ok(a);
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
    let session_id = format!(
        "session-{}",
        chrono_like_now()
    );
    let learn_cfg = LearnConfig::new(paths.root.clone(), session_id);
    let learn = match LearnHandles::build(learn_cfg) {
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
        let runner: arccode_tools::builtin::SubagentRunner = Arc::new(
            move |spec: arccode_tools::builtin::SubagentSpec| {
                let cfg = cfg_for_runner.clone();
                let mode = mode_for_runner;
                Box::pin(async move {
                    let sel = if spec.model.contains('/') {
                        let (p, m) = spec.model.split_once('/').unwrap();
                        Selection {
                            provider_id: p.to_string(),
                            model: m.to_string(),
                        }
                    } else {
                        resolve_selection(&cfg, None).map_err(|e| e.to_string())?
                    };
                    let provider = build_provider(&cfg, &sel.provider_id)
                        .map_err(|e| e.to_string())?;
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let paths = ProjectPaths::discover(&cwd);
                    let ctx = ToolCtx::new_with_config(
                        mode,
                        cwd,
                        paths.root.clone(),
                        cfg.tools.shell_denylist.clone(),
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
                            arccode_core::AgentEvent::TextDelta { text } => out.push_str(&text),
                            arccode_core::AgentEvent::Error { message } => return Err(message),
                            arccode_core::AgentEvent::Stop { .. } => break,
                            _ => {}
                        }
                    }
                    Ok(out)
                })
            },
        );
        registry.register_arc(Arc::new(arccode_tools::builtin::SpawnSubagent::new(runner)));
    }

    // Compose the system prompt: base + memory index + skills catalog.
    let memory_store = learn
        .as_ref()
        .map(|l| l.memory.clone())
        .unwrap_or_else(|| Arc::new(MemoryStore::new(paths.root.clone())));
    let skills = arccode_skills::load_all(&paths.root);
    let system = build_system_prompt_full(mode, &memory_store, &skills);

    let agent_cfg = AgentConfig {
        model: selection.model.clone(),
        system: Some(system),
        tool_output_budget: ToolOutputBudget::new(cfg.tokens.tool_output_max_lines),
        compactor: Compactor {
            trigger_tokens: cfg.tokens.compact_at_tokens,
            ..Default::default()
        },
        learning: learn
            .as_ref()
            .map(|l| l.hook.clone() as Arc<dyn arccode_core::LearningHook>),
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
/// uses `recall_memory` to fetch them on demand.
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
    if let Some(block) = arccode_learn::memory::render_prompt_block(&memories) {
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
        "You are arccode, a self-improving terminal coding agent. You help the user inspect, \
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
