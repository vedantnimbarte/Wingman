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
        "lmstudio" | "lm_studio" | "lm-studio" => ProviderSupport::Untested,
        "vllm" => ProviderSupport::Untested,
        "ollama" => ProviderSupport::Untested,
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
        // Future providers should not be blocked — they just get a
        // warning and the user can try them.
        assert_eq!(classify("xai"), ProviderSupport::Untested);
        assert_eq!(classify("groq"), ProviderSupport::Untested);
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
