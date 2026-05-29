//! Per-provider tool-call support classification.
//!
//! Pilot mode depends on the model emitting structured tool-use blocks.
//! Not every backend in `arccode-providers` reliably does that — some
//! local-model setups (LM Studio, vLLM, Ollama) will load any model the
//! user picks, including ones that have no tool-use training.
//!
//! This module gates `arccode pilot run` on a static support table:
//!
//! | Tier         | Behaviour                                                 |
//! | ------------ | --------------------------------------------------------- |
//! | `Native`     | First-class tool use (Anthropic, Gemini).                 |
//! | `Compat`     | OpenAI-compat `tool_calls` shape (OpenAI, ChatGPT,        |
//! |              | OpenRouter, LiteLLM). Works when the model itself does.   |
//! | `Untested`   | Backend forwards tool calls to OpenAI-compat shape, but   |
//! |              | the model's tool-use quality is per-user (LM Studio,      |
//! |              | vLLM, Ollama). We warn but don't block.                   |
//! | `Unsupported`| Known not to emit tool calls at all (none right now —     |
//! |              | placeholder for future backends).                         |
//!
//! Phase 8 ships the table and the warning/error logic. Actual live
//! validation across all nine providers (running the canned plan and
//! confirming end-to-end success) needs API keys and is documented in
//! README as something the user runs themselves.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSupport {
    Native,
    Compat,
    Untested,
    Unsupported,
}

impl fmt::Display for ProviderSupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Native => "native",
            Self::Compat => "openai-compat",
            Self::Untested => "untested (model-dependent)",
            Self::Unsupported => "unsupported",
        })
    }
}

/// Classify the named provider for pilot mode.
///
/// Unknown ids return [`ProviderSupport::Untested`] so a future or
/// user-installed provider doesn't get blocked just because it's not in
/// the static table — the user sees a warning, not a hard error.
pub fn classify(provider_id: &str) -> ProviderSupport {
    match provider_id.to_ascii_lowercase().as_str() {
        "anthropic" => ProviderSupport::Native,
        "gemini" => ProviderSupport::Native,
        "openai" => ProviderSupport::Compat,
        "chatgpt" => ProviderSupport::Compat,
        "openrouter" => ProviderSupport::Compat,
        "litellm" => ProviderSupport::Compat,
        // Hosted OpenAI-shape clouds with documented tool-calling support.
        "groq" => ProviderSupport::Compat,
        "together" | "togetherai" | "together_ai" => ProviderSupport::Compat,
        "fireworks" | "fireworks_ai" | "fireworksai" => ProviderSupport::Compat,
        "deepinfra" => ProviderSupport::Compat,
        "xai" | "grok" => ProviderSupport::Compat,
        "deepseek" => ProviderSupport::Compat,
        "mistral" | "mistralai" => ProviderSupport::Compat,
        "cerebras" => ProviderSupport::Compat,
        "sambanova" => ProviderSupport::Compat,
        "azure" | "azure_openai" | "azureopenai" => ProviderSupport::Compat,
        "github" | "github_models" | "githubmodels" => ProviderSupport::Compat,
        // Perplexity Sonar is search-augmented; tool-use support is
        // model-dependent and not guaranteed — warn but don't block.
        "perplexity" | "pplx" => ProviderSupport::Untested,
        "lmstudio" | "lm_studio" | "lm-studio" => ProviderSupport::Untested,
        "vllm" => ProviderSupport::Untested,
        "ollama" => ProviderSupport::Untested,
        "llamacpp" | "llama_cpp" | "llama-cpp" => ProviderSupport::Untested,
        "tgi" | "hf_tgi" => ProviderSupport::Untested,
        // Wave 2 hosted clouds. Llama/Qwen-Coder on OpenAI-shape tool_calls.
        "anyscale" => ProviderSupport::Compat,
        "lepton" | "leptonai" => ProviderSupport::Compat,
        "novita" => ProviderSupport::Compat,
        "hyperbolic" => ProviderSupport::Compat,
        "lambda" | "lambdalabs" => ProviderSupport::Compat,
        "nebius" => ProviderSupport::Compat,
        "hf" | "huggingface" | "hf_inference" => ProviderSupport::Compat,
        "nvidia" | "nim" | "nvidia_nim" => ProviderSupport::Compat,
        "databricks" => ProviderSupport::Compat,
        "snowflake" | "cortex" => ProviderSupport::Compat,
        // Cohere (native adapter) supports tool calls natively.
        "cohere" => ProviderSupport::Native,
        // Replicate's proxy + the long-tail OSS hosts are model-dependent;
        // mark untested so users get a warning, not a block.
        "replicate" => ProviderSupport::Untested,
        "glhf" => ProviderSupport::Untested,
        "featherless" => ProviderSupport::Untested,
        "octoai" => ProviderSupport::Untested,
        "avian" => ProviderSupport::Untested,
        "kluster" => ProviderSupport::Untested,
        "inferencenet" | "inference_net" => ProviderSupport::Untested,
        "writer" | "palmyra" => ProviderSupport::Untested,
        // Wave 2 local runtimes.
        "gpt4all" => ProviderSupport::Untested,
        "jan" | "janai" => ProviderSupport::Untested,
        "koboldcpp" | "kobold" => ProviderSupport::Untested,
        "oobabooga" | "ooba" | "textgenwebui" => ProviderSupport::Untested,
        // Wave 3 Chinese clouds. Qwen, GLM, Moonshot, Doubao, MiniMax all
        // ship documented OpenAI-style tool_calls support.
        "qwen" | "dashscope" | "alibaba" => ProviderSupport::Compat,
        "zhipu" | "glm" | "bigmodel" => ProviderSupport::Compat,
        "moonshot" | "kimi" => ProviderSupport::Compat,
        "minimax" => ProviderSupport::Compat,
        "doubao" | "volcengine" | "bytedance" | "ark" => ProviderSupport::Compat,
        "siliconflow" | "silicon" => ProviderSupport::Compat,
        // Untested: tool-use is model-dependent on these.
        "yi" | "lingyiwanwu" | "01ai" => ProviderSupport::Untested,
        "baichuan" => ProviderSupport::Untested,
        "hunyuan" | "tencent" => ProviderSupport::Untested,
        // Aggregators: gateways pass-through to OpenAI/Anthropic shape so
        // their support tracks the underlying model.
        "cloudflare" | "workersai" | "workers_ai" => ProviderSupport::Compat,
        "vercel" | "vercel_gateway" => ProviderSupport::Compat,
        "aimlapi" | "aiml" => ProviderSupport::Compat,
        "openpipe" => ProviderSupport::Compat,
        "targon" => ProviderSupport::Untested,
        "pollinations" => ProviderSupport::Untested,
        // Other hosted.
        "ai21" | "jamba" => ProviderSupport::Compat,
        "zai" | "z_ai" | "z-ai" => ProviderSupport::Compat,
        "friendli" | "friendliai" => ProviderSupport::Compat,
        "reka" => ProviderSupport::Compat,
        "mancer" => ProviderSupport::Untested,
        // Wave 3 local runtimes.
        "mlx" | "mlx_lm" | "mlxlm" => ProviderSupport::Untested,
        "localai" | "local_ai" => ProviderSupport::Untested,
        "aphrodite" => ProviderSupport::Untested,
        "mistralrs" | "mistral_rs" => ProviderSupport::Untested,
        _ => ProviderSupport::Untested,
    }
}

/// Render a user-facing line summarising the support tier. Used by
/// `arccode pilot run`'s startup banner.
pub fn support_notice(provider_id: &str) -> String {
    let tier = classify(provider_id);
    match tier {
        ProviderSupport::Native => format!(
            "[pilot] provider {provider_id}: {tier} tool use — full pilot support."
        ),
        ProviderSupport::Compat => format!(
            "[pilot] provider {provider_id}: {tier} — should work if the model itself supports tool use."
        ),
        ProviderSupport::Untested => format!(
            "[pilot] provider {provider_id}: {tier} — pilot quality depends on the local model. \
             If tasks stall with no tool calls, switch to a model with proven tool-use training."
        ),
        ProviderSupport::Unsupported => format!(
            "[pilot] provider {provider_id}: {tier} — pilot mode requires structured tool use. \
             Switch providers before running."
        ),
    }
}

/// Returns `Err(reason)` if pilot mode should refuse to start against
/// this provider. Currently only `Unsupported` blocks; `Untested` warns
/// via the banner but doesn't gate the run.
pub fn gate_run(provider_id: &str) -> Result<(), String> {
    if classify(provider_id) == ProviderSupport::Unsupported {
        Err(format!(
            "provider {provider_id} is marked unsupported for pilot mode (no structured tool use). \
             Use --model <other-provider>/<model> or `arccode config set default_provider`."
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_providers_classified() {
        assert_eq!(classify("anthropic"), ProviderSupport::Native);
        assert_eq!(classify("gemini"), ProviderSupport::Native);
        assert_eq!(classify("openai"), ProviderSupport::Compat);
        assert_eq!(classify("chatgpt"), ProviderSupport::Compat);
        assert_eq!(classify("openrouter"), ProviderSupport::Compat);
        assert_eq!(classify("litellm"), ProviderSupport::Compat);
        assert_eq!(classify("lmstudio"), ProviderSupport::Untested);
        assert_eq!(classify("vllm"), ProviderSupport::Untested);
        assert_eq!(classify("ollama"), ProviderSupport::Untested);
    }

    #[test]
    fn unknown_provider_is_untested_not_unsupported() {
        // Truly-unknown providers should not be blocked — they just get a
        // warning and the user can try them.
        assert_eq!(classify("brand-new-llm-host"), ProviderSupport::Untested);
    }

    #[test]
    fn new_hosted_clouds_are_compat() {
        for id in [
            "groq",
            "together",
            "fireworks",
            "deepinfra",
            "xai",
            "deepseek",
            "mistral",
            "cerebras",
            "sambanova",
            "azure",
            "github",
        ] {
            assert_eq!(
                classify(id),
                ProviderSupport::Compat,
                "{id} should be Compat for pilot"
            );
        }
    }

    #[test]
    fn perplexity_is_untested_not_compat() {
        // Sonar models are search-augmented; tool-use is not guaranteed.
        assert_eq!(classify("perplexity"), ProviderSupport::Untested);
    }

    #[test]
    fn new_local_runtimes_are_untested() {
        for id in [
            "llamacpp",
            "tgi",
            "gpt4all",
            "jan",
            "koboldcpp",
            "oobabooga",
        ] {
            assert_eq!(classify(id), ProviderSupport::Untested);
        }
    }

    #[test]
    fn cohere_is_native() {
        assert_eq!(classify("cohere"), ProviderSupport::Native);
    }

    #[test]
    fn wave3_chinese_clouds_classified() {
        for id in ["qwen", "zhipu", "moonshot", "minimax", "doubao", "siliconflow"] {
            assert_eq!(classify(id), ProviderSupport::Compat, "{id}");
        }
        for id in ["yi", "baichuan", "hunyuan"] {
            assert_eq!(classify(id), ProviderSupport::Untested, "{id}");
        }
    }

    #[test]
    fn wave3_aggregators_classified() {
        for id in ["cloudflare", "vercel", "aimlapi", "openpipe"] {
            assert_eq!(classify(id), ProviderSupport::Compat, "{id}");
        }
    }

    #[test]
    fn wave3_other_hosted_classified() {
        for id in ["ai21", "zai", "friendli", "reka"] {
            assert_eq!(classify(id), ProviderSupport::Compat, "{id}");
        }
    }

    #[test]
    fn wave2_hosted_clouds_are_compat() {
        for id in [
            "anyscale",
            "lepton",
            "novita",
            "hyperbolic",
            "lambda",
            "nebius",
            "hf",
            "nvidia",
            "databricks",
            "snowflake",
        ] {
            assert_eq!(
                classify(id),
                ProviderSupport::Compat,
                "{id} should be Compat"
            );
        }
    }

    #[test]
    fn provider_id_case_folded() {
        assert_eq!(classify("Anthropic"), ProviderSupport::Native);
        assert_eq!(classify("LM_STUDIO"), ProviderSupport::Untested);
    }

    #[test]
    fn gate_run_passes_for_supported_tiers() {
        assert!(gate_run("anthropic").is_ok());
        assert!(gate_run("ollama").is_ok());
    }

    #[test]
    fn support_notice_mentions_tier() {
        assert!(support_notice("anthropic").contains("native"));
        assert!(support_notice("openai").contains("compat"));
        assert!(support_notice("ollama").contains("untested"));
    }
}
