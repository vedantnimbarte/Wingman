//! wingman provider implementations.
//!
//! Each provider translates the provider-agnostic `wingman_core::Message` /
//! `CompletionRequest` shape into its native wire format and back. The
//! Anthropic implementation is the reference — it exercises every feature
//! (streaming, tool use, explicit prompt caching). The OpenAI-compatible
//! adapter covers six API-shape clones (OpenAI, OpenRouter, LM Studio,
//! vLLM, LiteLLM, Ollama) via a single struct.

pub mod anthropic;
pub mod chatgpt;
pub mod cohere;
pub mod gemini;
pub mod openai_compat;
pub mod probe;
mod retry;
pub mod watsonx;

pub use anthropic::AnthropicProvider;
pub use chatgpt::ChatGptProvider;
pub use cohere::CohereProvider;
pub use gemini::GeminiProvider;
pub use openai_compat::{OpenAiCompatProvider, Variant as OpenAiVariant};
pub use probe::probe;
pub use watsonx::{WatsonxCredential, WatsonxProvider};
