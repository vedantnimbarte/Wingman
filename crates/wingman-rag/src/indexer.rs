//! Walker + chunker + embedder + store, glued together.
//!
//! Call [`Indexer::reindex_repo`] once at startup. For incremental updates
//! call [`Indexer::reindex_file`] on a changed path (the file watcher in
//! `wingman-cli` drives this). Each file's content hash is stored so an
//! unchanged file is a no-op.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::WalkBuilder;

use crate::chunker::{is_indexable_file, Chunk, Chunker};
use crate::{Embedder, IndexStore, Result, ScoredChunk};

#[derive(Debug, Default, Clone, Copy)]
pub struct IndexStats {
    pub files_scanned: u32,
    pub files_indexed: u32,
    pub chunks_written: u32,
}

pub struct Indexer {
    root: PathBuf,
    chunker: Chunker,
    embedder: Arc<dyn Embedder>,
    store: Arc<IndexStore>,
    batch_size: usize,
}

impl Indexer {
    pub fn new(root: PathBuf, embedder: Arc<dyn Embedder>, store: Arc<IndexStore>) -> Self {
        Self {
            root,
            chunker: Chunker::default(),
            embedder,
            store,
            batch_size: 32,
        }
    }

    pub fn with_chunker(mut self, c: Chunker) -> Self {
        self.chunker = c;
        self
    }

    pub fn store(&self) -> &Arc<IndexStore> {
        &self.store
    }

    pub fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
    }

    /// Walk the repo (respecting `.gitignore`) and bring the index up to
    /// date. Unchanged files (same content hash) are skipped.
    pub async fn reindex_repo(&self) -> Result<IndexStats> {
        let mut stats = IndexStats::default();
        let walker = WalkBuilder::new(&self.root).build();
        for entry in walker.flatten() {
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                continue;
            }
            stats.files_scanned += 1;
            let path = entry.path();
            if let Some(n) = self.reindex_path(path).await? {
                stats.files_indexed += 1;
                stats.chunks_written += n;
            }
        }
        Ok(stats)
    }

    /// Re-index a single file (used by the watcher). Returns the number of
    /// chunks written, or `None` if the file was skipped (binary, large, or
    /// unchanged hash).
    pub async fn reindex_file(&self, abs_path: &Path) -> Result<Option<u32>> {
        self.reindex_path(abs_path).await
    }

    async fn reindex_path(&self, abs_path: &Path) -> Result<Option<u32>> {
        let Some(rel) = relativize(&self.root, abs_path) else {
            return Ok(None);
        };
        let bytes = match std::fs::read(abs_path) {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        if !is_indexable_file(abs_path, &bytes) {
            // Was it in the index previously? Drop it.
            self.store.forget(&rel)?;
            return Ok(None);
        }
        let hash = blake3::hash(&bytes).to_hex().to_string();
        if let Some(existing) = self.store.file_hash(&rel)? {
            if existing == hash {
                return Ok(None);
            }
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let chunks = self.chunker.chunk(&rel, &text);
        if chunks.is_empty() {
            self.store.forget(&rel)?;
            return Ok(Some(0));
        }
        let embeddings = self.embed_in_batches(&chunks).await?;
        self.store.replace_file(&rel, &hash, &chunks, &embeddings)?;
        Ok(Some(chunks.len() as u32))
    }

    async fn embed_in_batches(&self, chunks: &[Chunk]) -> Result<Vec<Vec<f32>>> {
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(chunks.len());
        for batch in chunks.chunks(self.batch_size) {
            let texts: Vec<String> = batch.iter().map(|c| c.content.clone()).collect();
            let mut embs = self.embedder.embed(&texts).await?;
            out.append(&mut embs);
        }
        Ok(out)
    }

    /// Embed a query and run **hybrid** search — dense vector similarity fused
    /// with BM25 keyword scoring (RRF). Better recall on code than vector-only
    /// (it also catches exact identifier / error-string matches). Falls back to
    /// pure vector search via [`Self::search_vector`] if you need it.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<ScoredChunk>> {
        let embs = self.embedder.embed(&[query.to_string()]).await?;
        if embs.is_empty() {
            return Ok(Vec::new());
        }
        self.store.search_hybrid(query, &embs[0], limit)
    }

    /// Pure dense vector search (no keyword fusion).
    pub async fn search_vector(&self, query: &str, limit: usize) -> Result<Vec<ScoredChunk>> {
        let embs = self.embedder.embed(&[query.to_string()]).await?;
        if embs.is_empty() {
            return Ok(Vec::new());
        }
        self.store.search(&embs[0], limit)
    }
}

fn relativize(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}
