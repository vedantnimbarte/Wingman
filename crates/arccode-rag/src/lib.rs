//! arccode retrieval, embedding, and index layer.
//!
//! - [`Embedder`] is the trait every embedding backend implements.
//!   The default is [`FastembedEmbedder`] (BAAI/bge-small-en-v1.5 via
//!   `fastembed-rs`); a tiny deterministic [`HashEmbedder`] ships for tests
//!   and for users who can't run ONNX.
//! - [`Chunker`] splits source files into overlapping line windows.
//! - [`IndexStore`] persists chunks + embeddings to SQLite under
//!   `.arccode/index.db` and serves cosine-similarity queries.
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
pub use embedder::{Embedder, HashEmbedder};
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
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, RagError>;
