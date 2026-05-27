//! arccode provider implementations.
//!
//! Each provider translates the provider-agnostic `arccode_core::Message` /
//! `CompletionRequest` shape into its native wire format and back. The
//! Anthropic implementation is the reference — it exercises every feature
//! (streaming, tool use, explicit prompt caching). The OpenAI-compatible
//! adapter covers six API-shape clones (OpenAI, OpenRouter, LM Studio,
//! vLLM, LiteLLM, Ollama) via a single struct.

pub mod anthropic;
pub mod gemini;
pub mod openai_compat;
pub mod probe;

pub use anthropic::AnthropicProvider;
pub use gemini::GeminiProvider;
pub use openai_compat::{OpenAiCompatProvider, Variant as OpenAiVariant};
pub use probe::probe;
