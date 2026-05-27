//! `/login` modal task runner — bridges the TUI wizard to the runtime.
//!
//! The TUI dispatches a [`LoginTask`] (Probe or Commit) and awaits a
//! `Result<(), String>`. This module owns the side effects: building a
//! temporary provider for the probe, writing the keyring entry, and
//! persisting the new default provider+model to the global config file.

use arccode_config::{global_config_path, secrets, Config};
use arccode_core::Provider;
use arccode_providers::{
    probe, AnthropicProvider, GeminiProvider, OpenAiCompatProvider, OpenAiVariant,
};
use arccode_tui::modal::{LoginPayload, LoginTask};
use std::sync::Arc;

/// Entry point invoked by the TUI's login runner closure.
pub async fn run_login_task(task: LoginTask) -> Result<(), String> {
    match task {
        LoginTask::Probe(payload) => probe_payload(&payload).await,
        LoginTask::Commit(payload) => commit_payload(&payload).await,
    }
}

async fn probe_payload(p: &LoginPayload) -> Result<(), String> {
    let provider = build_provider(p)?;
    probe(&*provider, &p.model).await
}

async fn commit_payload(p: &LoginPayload) -> Result<(), String> {
    // 1. Persist the API key to the OS keyring, if the provider has one.
    if let Some(key) = p.api_key.as_deref() {
        secrets::store(&p.provider_id, key).map_err(|e| format!("keyring: {e}"))?;
    }

    // 2. Update the global config file to point at this provider+model and
    //    record either the keyring marker (for providers with a key) or the
    //    custom base URL (for local providers).
    let path = global_config_path().map_err(|e| format!("config path: {e}"))?;
    Config::set_default_provider_and_save(
        &path,
        &p.provider_id,
        &p.model,
        p.base_url.as_deref(),
        p.api_key.is_some(),
    )
    .map_err(|e| format!("save config: {e}"))?;

    Ok(())
}

/// Build a provider directly from a wizard payload, without consulting the
/// keyring or the on-disk config. Used by the probe so we test the key the
/// user just typed, not a previously stored one.
fn build_provider(p: &LoginPayload) -> Result<Arc<dyn Provider>, String> {
    let api_key = p.api_key.clone();
    let base_url = p.base_url.clone();
    let mk_err = |e: arccode_core::ArccodeError| format!("{e}");

    match p.provider_id.as_str() {
        "anthropic" => {
            let key = api_key.ok_or("anthropic requires an API key")?;
            let mut prov = AnthropicProvider::new(key).map_err(mk_err)?;
            if let Some(url) = base_url {
                prov = prov.with_base_url(url);
            }
            Ok(Arc::new(prov))
        }
        "gemini" => {
            let key = api_key.ok_or("gemini requires an API key")?;
            let mut prov = GeminiProvider::new(key).map_err(mk_err)?;
            if let Some(url) = base_url {
                prov = prov.with_base_url(url);
            }
            Ok(Arc::new(prov))
        }
        id => {
            let variant = openai_variant(id).ok_or_else(|| format!("unknown provider '{id}'"))?;
            let mut prov = OpenAiCompatProvider::new(variant, api_key).map_err(mk_err)?;
            if let Some(url) = base_url {
                prov = prov.with_base_url(url);
            }
            Ok(Arc::new(prov))
        }
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

