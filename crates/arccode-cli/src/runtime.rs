//! Wires config into a concrete `Provider` + `ToolRegistry` + `AgentLoop`.
//!
//! Keeps the per-provider plumbing in one place so command handlers
//! (headless --print, --json, future TUI) can just ask `Runtime::build(...)`.

use anyhow::{anyhow, Context, Result};
use arccode_config::{secrets, Config, PermissionMode, ProjectPaths};
use arccode_core::{AgentConfig, AgentLoop, Compactor, Provider, ToolOutputBudget};
use arccode_providers::{AnthropicProvider, GeminiProvider, OpenAiCompatProvider, OpenAiVariant};
use arccode_rag::{Embedder, HashEmbedder, IndexStore, Indexer};
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
            "provider '{other}' is not implemented yet (M2 ships Anthropic + OpenAI/OpenRouter/LM Studio/vLLM/LiteLLM/Ollama; Gemini next)"
        )),
    }
}

fn openai_variant(id: &str) -> Option<OpenAiVariant> {
    Some(match id {
        "openai" => OpenAiVariant::OpenAI,
        "openrouter" => OpenAiVariant::OpenRouter,
        "lmstudio" | "lm_studio" => OpenAiVariant::LmStudio,
        "vllm" => OpenAiVariant::Vllm,
        "litellm" => OpenAiVariant::LiteLlm,
        "ollama" => OpenAiVariant::Ollama,
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
        OpenAiVariant::LmStudio | OpenAiVariant::Vllm | OpenAiVariant::Ollama => return None,
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

pub async fn build_registry(_cfg: &Config, mode: PermissionMode) -> Result<ToolRegistry> {
    let cwd = std::env::current_dir()?;
    let paths = ProjectPaths::discover(&cwd);
    let ctx = ToolCtx::new(mode, cwd, paths.root.clone());
    let mut reg = ToolRegistry::new(ctx).with_builtins();
    if let Some(indexer) = build_indexer(&paths)? {
        reg = reg.with_semantic_search(indexer);
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

/// Variant that also returns the shared `Arc<ToolRegistry>` so callers can
/// register/unregister tools at runtime (used by the MCP registry).
pub async fn build_agent_and_registry(
    cfg: &Config,
    selection: &Selection,
    mode: PermissionMode,
) -> Result<(AgentLoop, Arc<ToolRegistry>)> {
    let provider = build_provider(cfg, &selection.provider_id)?;
    let registry = Arc::new(build_registry(cfg, mode).await?);
    let system = build_system_prompt(mode);
    let agent_cfg = AgentConfig {
        model: selection.model.clone(),
        system: Some(system),
        tool_output_budget: ToolOutputBudget::new(cfg.tokens.tool_output_max_lines),
        compactor: Compactor {
            trigger_tokens: cfg.tokens.compact_at_tokens,
            ..Default::default()
        },
        ..Default::default()
    };
    let agent = AgentLoop::new(provider, registry.clone(), agent_cfg);
    Ok((agent, registry))
}

pub fn build_system_prompt(mode: PermissionMode) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    format!(
        "You are arccode, a terminal coding agent. You help the user inspect, \
         edit, and run code from the command line.\n\
         \n\
         Available tools: semantic_search, read_file, write_file, edit_file, run_shell, list_dir, glob, grep.\n\
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
         Environment:\n\
         - Working directory: {cwd}\n\
         - Permission mode: {mode}\n"
    )
}
