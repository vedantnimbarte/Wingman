use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Tokens written to cache this request (Anthropic cache_creation).
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    /// Tokens served from cache this request (Anthropic cache_read).
    #[serde(default)]
    pub cache_read_input_tokens: u32,
}

impl Usage {
    /// Tokens this turn actually billed at "fresh input" price.
    pub fn billable_input(&self) -> u32 {
        self.input_tokens
    }

    /// 0.0..=1.0 — fraction of input tokens served from cache.
    pub fn cache_hit_ratio(&self) -> f32 {
        let total =
            self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens;
        if total == 0 {
            return 0.0;
        }
        self.cache_read_input_tokens as f32 / total as f32
    }

    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
    }
}
