//! wingman tools layer.
//!
//! - [`Tool`] is the trait each built-in or external tool implements.
//! - [`ToolCtx`] carries permission mode, cwd, and project root into every
//!   call so tools can decide whether to act, prompt, or refuse.
//! - [`ToolRegistry`] holds the registered tools and implements
//!   `wingman_core::ToolDispatcher`, the trait the agent loop calls into.

mod ctx;
mod registry;

pub mod builtin;
pub mod prefetch;

pub use ctx::ToolCtx;
pub use registry::{run_hook, HookResult, ToolRegistry};

use async_trait::async_trait;
use serde_json::Value;
use wingman_core::{ToolOutcome, ToolSpec};

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome;
}
