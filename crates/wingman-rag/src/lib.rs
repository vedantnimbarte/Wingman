//! wingman retrieval, embedding, and index layer.
//!
//! - [`Embedder`] is the trait every embedding backend implements.
//!   The default is [`FastembedEmbedder`] (BAAI/bge-small-en-v1.5 via
//!   `fastembed-rs`); a tiny deterministic [`HashEmbedder`] ships for tests
//!   and for users who can't run ONNX.
//! - [`Chunker`] splits source files into overlapping line windows.
//! - [`IndexStore`] persists chunks + embeddings to SQLite under
//!   `.wingman/index.db` and serves cosine-similarity queries.
//! - [`Indexer`] orchestrates walker + chunker + embedder + store and is
//!   what callers actually drive.

mod chunker;
mod embedder;
mod indexer;
mod store;
mod watcher;

pub use chunker::{Chunk, Chunker};
#[cfg(feature = "embeddings")]
pub use embedder::FastembedEmbedder;
pub use embedder::{Embedder, HashEmbedder, LazyEmbedder};
pub use indexer::{IndexStats, Indexer};
pub use store::{IndexStore, ScoredChunk};
pub use watcher::{spawn_background_indexer, WatcherHandle};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RagError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sql: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("embedder: {0}")]
    Embedder(String),
    #[error("dim mismatch: index has {expected}, embedder produces {actual}")]
    DimMismatch { expected: usize, actual: usize },
    /// The index was stamped by a different embedding model. Separate from
    /// [`RagError::DimMismatch`] because two models can agree on dimension and
    /// still produce vectors that mean nothing to each other — reporting that
    /// as a dim mismatch prints "4-dim vs 4-dim" and tells nobody anything.
    #[error("embedder changed: index was built by {expected}, this session uses {actual}")]
    EmbedderChanged { expected: String, actual: String },
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, RagError>;
