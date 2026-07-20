//! `/login` modal task runner — bridges the TUI wizard to the runtime.
//!
//! The TUI dispatches a [`LoginTask`] (Probe or Commit) and awaits a
//! `Result<(), String>`. This module owns the side effects: building a
//! temporary provider for the probe, writing the keyring entry, and
//! persisting the new default provider+model to the global config file.

use std::sync::Arc;
use wingman_config::{global_config_path, secrets, Config};
use wingman_core::Provider;
use wingman_providers::{
    probe, AnthropicProvider, ChatGptProvider, CohereProvider, GeminiProvider,
    OpenAiCompatProvider, OpenAiVariant, WatsonxCredential, WatsonxProvider,
};
use wingman_tui::modal::{LoginPayload, LoginTask};

/// Entry point invoked by the TUI's login runner closure.
pub async fn run_login_task(task: LoginTask) -> Result<(), String> {
    match task {
        LoginTask::Probe(payload) => probe_payload(&payload).await,
        LoginTask::Commit(payload) => commit_payload(&payload).await,
        // OAuthLogin is fully handled in cli.rs's login_runner before reaching here.
        LoginTask::OAuthLogin { provider_id } => Err(format!(
            "unexpected OAuthLogin for '{provider_id}' in login task runner"
        )),
    }
}

async fn probe_payload(p: &LoginPayload) -> Result<(), String> {
    let provider = build_provider(p)?;
    probe(&*provider, &p.model).await
}

async fn commit_payload(p: &LoginPayload) -> Result<(), String> {
    let path = global_config_path().map_err(|e| format!("config path: {e}"))?;

    // Prefer the OS keyring: store the secret there and write only a
    // `keyring:<provider>` marker to the config. Fall back to a plaintext key
    // in the config (locked to 0600 by save_atomic) only when the keyring is
    // unavailable, so a headless / no-keyring box still works.
    let stored_in_keyring = match p.api_key.as_deref() {
        Some(key) => {
            let provider_id = p.provider_id.clone();
            let key = key.to_string();
            match tokio::task::spawn_blocking(move || secrets::store(&provider_id, &key)).await {
                Ok(Ok(())) => true,
                Ok(Err(e)) => {
                    eprintln!("[wingman] keyring store failed, keeping key in config: {e}");
                    false
                }
                Err(e) => {
                    eprintln!("[wingman] keyring store task failed, keeping key in config: {e}");
                    false
                }
            }
        }
        None => false,
    };

    if stored_in_keyring {
        Config::set_default_provider_and_save(
            &path,
            &p.provider_id,
            &p.model,
            p.base_url.as_deref(),
            true,
        )
        .map_err(|e| format!("save config: {e}"))
    } else {
        Config::set_default_provider_and_save_with_key(
            &path,
            &p.provider_id,
            &p.model,
            p.base_url.as_deref(),
            p.api_key.as_deref(),
        )
        .map_err(|e| format!("save config: {e}"))
    }
}

/// Build a provider directly from a wizard payload, without consulting the
/// keyring or the on-disk config. Used by the probe so we test the key the
/// user just typed, not a previously stored one.
fn build_provider(p: &LoginPayload) -> Result<Arc<dyn Provider>, String> {
    let api_key = p.api_key.clone();
    let base_url = p.base_url.clone();
    let mk_err = |e: wingman_core::WingmanError| format!("{e}");

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
        "chatgpt" => {
            // The OAuth runner stored the token in keychain; read it back.
            let token = api_key
                .or_else(|| secrets::load("chatgpt").ok().flatten())
                .ok_or("chatgpt: no access token found — complete browser login first")?;
            Ok(Arc::new(ChatGptProvider::new(token).map_err(mk_err)?))
        }
        "cohere" => {
            let key = api_key.ok_or("cohere requires an API key")?;
            let mut prov = CohereProvider::new(key).map_err(mk_err)?;
            if let Some(url) = base_url {
                prov = prov.with_base_url(url);
            }
            Ok(Arc::new(prov))
        }
        "watsonx" => {
            // Wizard probe: the user gave us an API key; we still need a
            // project id to talk to watsonx. Read it from the env (the
            // commit step is what persists it to config).
            let project_id = std::env::var("WATSONX_PROJECT_ID").map_err(|_| {
                "watsonx requires WATSONX_PROJECT_ID env var to be set during the login probe"
                    .to_string()
            })?;
            let key = api_key.ok_or("watsonx requires an API key")?;
            let mut prov =
                WatsonxProvider::new(WatsonxCredential::ApiKey(key), project_id).map_err(mk_err)?;
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
