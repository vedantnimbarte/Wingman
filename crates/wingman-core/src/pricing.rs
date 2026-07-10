//! Static per-model pricing for cost estimates.
//!
//! Numbers are USD per **million tokens** at list price. They drift over
//! time — treat the table as an order-of-magnitude estimate, not an
//! invoice. Local providers (Ollama, LM Studio, vLLM) have zero entries
//! since the user pays no per-token fee.
//!
//! Unknown models return `None`; the UI renders that as "—".

use crate::Usage;

/// Per-million-token prices in USD.
#[derive(Debug, Clone, Copy)]
pub struct Price {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    /// What it costs to *write* to the prompt cache (Anthropic only).
    pub cache_write_per_mtok: f64,
    /// What it costs to *read* from the prompt cache.
    pub cache_read_per_mtok: f64,
}

impl Price {
    /// Estimate the USD cost of `usage` at these rates.
    pub fn cost(&self, u: &Usage) -> f64 {
        let m = 1_000_000.0;
        (u.input_tokens as f64) * self.input_per_mtok / m
            + (u.output_tokens as f64) * self.output_per_mtok / m
            + (u.cache_creation_input_tokens as f64) * self.cache_write_per_mtok / m
            + (u.cache_read_input_tokens as f64) * self.cache_read_per_mtok / m
    }
}

/// Look up the price for a `provider/model` pair, or the bare model id.
///
/// The lookup is case-insensitive and tolerates the `provider/` prefix
/// being absent (so `claude-opus-4-7` and `anthropic/claude-opus-4-7`
/// both resolve).
pub fn price_for(key: &str) -> Option<Price> {
    let lower = key.to_ascii_lowercase();
    let model = lower.rsplit('/').next().unwrap_or(&lower);
    Some(match model {
        // Anthropic. Opus and Sonnet tiers have held the same list price
        // across the 4.x/5 generations; keep old ids resolving too.
        "claude-opus-4-8" | "claude-opus-4-7" => Price {
            input_per_mtok: 15.0,
            output_per_mtok: 75.0,
            cache_write_per_mtok: 18.75,
            cache_read_per_mtok: 1.50,
        },
        "claude-sonnet-5" | "claude-sonnet-4-6" => Price {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_write_per_mtok: 3.75,
            cache_read_per_mtok: 0.30,
        },
        "claude-haiku-4-5-20251001" | "claude-haiku-4-5" => Price {
            input_per_mtok: 1.0,
            output_per_mtok: 5.0,
            cache_write_per_mtok: 1.25,
            cache_read_per_mtok: 0.10,
        },

        // OpenAI. gpt-5 auto-caches input (no separate write fee).
        "gpt-5" => Price {
            input_per_mtok: 1.25,
            output_per_mtok: 10.0,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 0.125,
        },
        "gpt-5-mini" => Price {
            input_per_mtok: 0.25,
            output_per_mtok: 2.0,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 0.025,
        },
        "gpt-4.1" => Price {
            input_per_mtok: 2.50,
            output_per_mtok: 10.0,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 1.25,
        },
        "gpt-4o" => Price {
            input_per_mtok: 2.50,
            output_per_mtok: 10.0,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 1.25,
        },
        "gpt-4o-mini" => Price {
            input_per_mtok: 0.15,
            output_per_mtok: 0.60,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 0.075,
        },
        "o4-mini" => Price {
            input_per_mtok: 1.10,
            output_per_mtok: 4.40,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 0.275,
        },

        // Google
        "gemini-2.5-pro" => Price {
            input_per_mtok: 1.25,
            output_per_mtok: 10.0,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 0.31,
        },
        "gemini-2.5-flash" => Price {
            input_per_mtok: 0.30,
            output_per_mtok: 2.50,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 0.075,
        },
        "gemini-1.5-pro" => Price {
            input_per_mtok: 1.25,
            output_per_mtok: 5.0,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 0.31,
        },

        // DeepSeek V4 (hosted via OpenRouter / DeepSeek direct). These
        // endpoints don't bill a separate prompt-cache fee, so the cache
        // columns are zero. Without an entry here spend prices as $0, which
        // silently disables the pilot `max_usd` cap for the model.
        "deepseek-v4-pro" => Price {
            input_per_mtok: 0.435,
            output_per_mtok: 0.87,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 0.0,
        },
        "deepseek-v4-flash" => Price {
            input_per_mtok: 0.09,
            output_per_mtok: 0.18,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 0.0,
        },

        // Local — no per-token cost.
        "local-model" | "llama3.1:8b" | "qwen2.5:7b" | "deepseek-r1:8b" => Price {
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 0.0,
        },

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_default_models_are_priced() {
        // Regression: the app's live defaults must resolve, prefixed or not.
        for m in ["claude-opus-4-8", "anthropic/claude-sonnet-5", "gpt-5"] {
            assert!(price_for(m).is_some(), "{m} should be priced");
        }
    }

    #[test]
    fn deepseek_v4_resolves_through_the_openrouter_prefix() {
        // The pilot passes `openrouter/deepseek/deepseek-v4-pro`; the bare
        // model must still price so the max_usd cap actually bounds it.
        let p = price_for("openrouter/deepseek/deepseek-v4-pro").expect("v4-pro priced");
        assert!(p.output_per_mtok > p.input_per_mtok);
        assert!(price_for("deepseek/deepseek-v4-flash").is_some());
    }

    #[test]
    fn cache_read_billed_below_input() {
        let p = price_for("claude-opus-4-8").unwrap();
        let fresh = Usage { input_tokens: 1000, ..Default::default() };
        let cached = Usage { cache_read_input_tokens: 1000, ..Default::default() };
        assert!(p.cost(&cached) < p.cost(&fresh));
    }
}
