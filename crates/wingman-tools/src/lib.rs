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
pub use Capability as ToolCapability;

use async_trait::async_trait;
use serde_json::Value;
use wingman_core::{ToolOutcome, ToolSpec};

/// What a tool is capable of doing, declared up front so the registry can gate
/// it centrally instead of trusting each tool to check `ToolCtx` itself.
///
/// Enforcement used to be per-tool opt-in, which meant a tool that simply
/// forgot to consult `ToolCtx` was unguarded — and several did, writing files
/// in `read-only` mode. `ToolRegistry::dispatch` now refuses a call whose
/// declared capabilities the active permission mode doesn't grant, so the
/// default for a newly added tool is *deny*, not *allow*.
///
/// This is a coarse gate: it answers "may this tool write anything at all
/// right now". Tools still perform their own path containment (`allows_read`
/// / `allows_write` on the specific path) for the fine-grained question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capability(u8);

impl Capability {
    /// Pure computation — no filesystem, no shell, no network.
    pub const NONE: Self = Self(0);
    /// Reads file content from disk.
    pub const READ: Self = Self(1);
    /// Creates, modifies, or deletes files.
    pub const WRITE: Self = Self(2);
    /// Executes a subprocess.
    pub const SHELL: Self = Self(4);
    /// Makes outbound network requests.
    pub const NETWORK: Self = Self(8);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Capability {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome;

    /// What this tool may do. Defaults to [`Capability::NONE`] so a tool that
    /// declares nothing can only do pure computation; anything touching the
    /// filesystem, a subprocess, or the network must say so and will be gated
    /// on it. See [`Capability`].
    fn capabilities(&self) -> Capability {
        Capability::NONE
    }
}
