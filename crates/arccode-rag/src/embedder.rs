//! Embedding backends.
//!
//! All embedders speak the same async trait. [`FastembedEmbedder`] (gated by
//! the `embeddings` feature) is the real one; [`HashEmbedder`] is a tiny
//! deterministic fallback used in tests and as a no-deps option.

use async_trait::async_trait;

use crate::Result;

#[async_trait]
pub trait Embedder: Send + Sync {
    /// Stable id for telemetry/debug.
    fn id(&self) -> &str;
    /// Vector dimensionality; same for every output.
    fn dim(&self) -> usize;
    /// Embed a batch. Output length matches input length.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

// ---------------------------------------------------------------------------
// Deterministic fallback — token hashing into a fixed dim. Quality is poor
// but it's free, dependency-light, and good enough for unit tests.
// ---------------------------------------------------------------------------

pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(8) }
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new(64)
    }
}

#[async_trait]
impl Embedder for HashEmbedder {
    fn id(&self) -> &str {
        "hash"
    }
    fn dim(&self) -> usize {
        self.dim
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| hash_embed(t, self.dim)).collect())
    }
}

fn hash_embed(text: &str, dim: usize) -> Vec<f32> {
    let mut v = vec![0f32; dim];
    // Bag-of-tokens projected into `dim` buckets via blake3.
    for tok in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        let lower = tok.to_ascii_lowercase();
        let h = blake3::hash(lower.as_bytes());
        let bytes = h.as_bytes();
        // Use first 8 bytes as bucket index, next byte for sign.
        let bucket = u64::from_le_bytes(bytes[..8].try_into().unwrap()) as usize % dim;
        let sign = if bytes[8] & 1 == 0 { 1.0 } else { -1.0 };
        v[bucket] += sign;
    }
    // L2-normalize.
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

// ---------------------------------------------------------------------------
// fastembed-rs (feature-gated).
// ---------------------------------------------------------------------------

#[cfg(feature = "embeddings")]
mod fastembed_impl {
    use super::*;
    use crate::RagError;
    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// Default model: BAAI/bge-small-en-v1.5 — 384 dims, ~120 MB ONNX file,
    /// strong recall/speed tradeoff for English code and prose.
    pub struct FastembedEmbedder {
        model: Arc<Mutex<TextEmbedding>>,
        dim: usize,
        id: String,
    }

    impl FastembedEmbedder {
        pub fn new(cache_dir: Option<PathBuf>) -> Result<Self> {
            let mut opts =
                InitOptions::new(EmbeddingModel::BGESmallENV15).with_show_download_progress(false);
            if let Some(dir) = cache_dir {
                opts = opts.with_cache_dir(dir);
            }
            let model = TextEmbedding::try_new(opts)
                .map_err(|e| RagError::Embedder(format!("fastembed init: {e}")))?;
            Ok(Self {
                model: Arc::new(Mutex::new(model)),
                dim: 384,
                id: "bge-small-en-v1.5".to_string(),
            })
        }
    }

    #[async_trait::async_trait]
    impl Embedder for FastembedEmbedder {
        fn id(&self) -> &str {
            &self.id
        }
        fn dim(&self) -> usize {
            self.dim
        }
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            // fastembed is sync; offload to a blocking thread so we don't
            // stall the async runtime on large batches.
            let texts: Vec<String> = texts.to_vec();
            let dim = self.dim;
            let model = self.model.clone();
            tokio::task::spawn_blocking(move || -> Result<Vec<Vec<f32>>> {
                let mut guard = model
                    .lock()
                    .map_err(|_| RagError::Embedder("poisoned mutex".into()))?;
                let out = guard
                    .embed(texts, None)
                    .map_err(|e| RagError::Embedder(format!("embed: {e}")))?;
                for v in &out {
                    if v.len() != dim {
                        return Err(RagError::Embedder(format!(
                            "expected dim {dim}, got {}",
                            v.len()
                        )));
                    }
                }
                Ok(out)
            })
            .await
            .map_err(|e| RagError::Embedder(format!("join: {e}")))?
        }
    }
}

#[cfg(feature = "embeddings")]
pub use fastembed_impl::FastembedEmbedder;
