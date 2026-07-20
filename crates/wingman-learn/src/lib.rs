//! Self-improving learning loop for wingman.
//!
//! Four cooperating pieces:
//!
//! - [`memory`] — persistent user/project/feedback/reference memories,
//!   stored as markdown with frontmatter under `~/.wingman/memory/`
//!   (global) or `<project>/.wingman/memory/` (project-scoped).
//! - [`stats`] — SQLite-backed skill usage tracking at
//!   `~/.wingman/learn.db`. Records each invoke + outcome
//!   ('success' | 'corrected' | 'unclear') so the engine can surface
//!   skills that need a rewrite.
//! - [`session_index`] — embeds finished sessions into the RAG store so
//!   the agent can recall "have we discussed this before?" across
//!   projects.
//! - [`hooks`] — the [`LearningHook`] impl that wires all of the above
//!   into the agent loop's before/after-turn hook points.

pub mod distill;
pub mod extract;
pub mod hooks;
pub mod memory;
pub mod proposal;
pub mod session_index;
pub mod staleness;
pub mod stats;

pub use hooks::LearnHook;
pub use memory::{Memory, MemoryDraft, MemoryScope, MemoryStore, MemoryType};
pub use stats::{Outcome, RoutingStat, StatsStore};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum LearnError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sql: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("config: {0}")]
    Config(#[from] wingman_config::ConfigError),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, LearnError>;
