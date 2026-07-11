//! wingman core types.
//!
//! These types are the contract every provider speaks. Modeled after
//! Anthropic's Messages API because it's the most expressive (tool_use +
//! tool_result blocks, explicit cache_control). Other providers translate
//! into and out of this shape in `wingman-providers`.

pub mod agent;
pub mod checkpoint;
pub mod error;
pub mod message;
pub mod pricing;
pub mod provider;
pub mod stream;
pub mod tokens;
pub mod tool;
pub mod usage;

pub use agent::{
    AgentConfig, AgentEvent, AgentLoop, AgentStop, GateReport, LearningHook, NoopLearningHook,
    ToolDispatcher, ToolOutcome, TurnGate,
};
pub use error::{WingmanError, Result};
pub use message::{ContentBlock, Message, Role};
pub use pricing::{price_for, Price};
pub use provider::{
    complete_text, CacheBreakpoint, CacheKind, CompletionRequest, Provider, ProviderCapabilities,
};
pub use stream::{ProviderEventStream, StopReason, StreamEvent};
pub use tokens::{
    estimate_history_tokens, estimate_tokens, CompactPlan, Compactor, ToolOutputBudget,
};
pub use tool::ToolSpec;
pub use usage::Usage;
