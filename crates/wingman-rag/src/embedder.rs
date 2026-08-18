//! Embedding backends.
//!
//! All embedders speak the same async trait. [`FastembedEmbedder`] (gated by
//! the `embeddings` feature) is the real one; [`HashEmbedder`] is a tiny
//! deterministic fallback used in tests and as a no-deps option;
//! [`LazyEmbedder`] wraps either so a process that never searches never pays
//! to build one.

use async_trait::async_trait;
use std::sync::Arc;

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
// Lazy wrapper — defers model construction until something actually embeds.
// ---------------------------------------------------------------------------

/// An embedder that reports its id and dimensionality immediately but does not
/// construct the underlying model until something actually embeds.
///
/// Opening an index needs only [`Embedder::id`] and [`Embedder::dim`] — two
/// constants — yet obtaining them meant constructing a real embedder, and for
/// the fastembed backend that is a ~120 MB ONNX model read off disk. Every
/// `wingman --print`, every pilot worker, and every HTTP turn paid it at
/// startup, including the great majority that never call `semantic_search`.
///
/// A failed construction is not cached, so a transient failure (cold model
/// cache, no network) is retried on the next call rather than poisoning the
/// process. It also stops a failure from silently degrading a whole session to
/// [`HashEmbedder`]: previously that swapped in a 64-dim embedder, stamped the
/// on-disk index with it, and left every later run to detect the mismatch and
/// rebuild. Now the index is stamped with the real model's identity up front
/// and the error surfaces when embedding is attempted.
pub struct LazyEmbedder {
    id: String,
    dim: usize,
    cell: tokio::sync::OnceCell<Arc<dyn Embedder>>,
    build: Arc<dyn Fn() -> Result<Arc<dyn Embedder>> + Send + Sync>,
}

impl LazyEmbedder {
    /// `id` and `dim` must match what `build` produces — they are what the
    /// index gets stamped with before the model exists.
    pub fn new(
        id: impl Into<String>,
        dim: usize,
        build: impl Fn() -> Result<Arc<dyn Embedder>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            dim,
            cell: tokio::sync::OnceCell::new(),
            build: Arc::new(build),
        }
    }

    /// Whether the underlying model has been constructed yet. The point of
    /// this type is that it stays false on a run that never searches.
    pub fn is_loaded(&self) -> bool {
        self.cell.initialized()
    }
}

#[async_trait]
impl Embedder for LazyEmbedder {
    fn id(&self) -> &str {
        &self.id
    }
    fn dim(&self) -> usize {
        self.dim
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let inner = self
            .cell
            .get_or_try_init(|| {
                let build = self.build.clone();
                async move {
                    // Construction is blocking CPU and file I/O — keep it off
                    // the async runtime's worker threads.
                    tokio::task::spawn_blocking(move || build())
                        .await
                        .map_err(|e| crate::RagError::Embedder(format!("embedder init: {e}")))?
                }
            })
            .await?;
        inner.embed(texts).await
    }
}

#[cfg(test)]
mod lazy_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Reporting id/dim must not construct anything — that is the entire
    /// point: opening an index asks only those two questions.
    #[tokio::test]
    async fn metadata_does_not_build_the_model() {
        let builds = Arc::new(AtomicUsize::new(0));
        let counter = builds.clone();
        let lazy = LazyEmbedder::new("bge-small-en-v1.5", 384, move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(HashEmbedder::new(384)) as Arc<dyn Embedder>)
        });

        assert_eq!(lazy.id(), "bge-small-en-v1.5");
        assert_eq!(lazy.dim(), 384);
        assert!(!lazy.is_loaded());
        assert_eq!(builds.load(Ordering::SeqCst), 0);

        let out = lazy.embed(&["hello".to_string()]).await.unwrap();
        assert_eq!(out.len(), 1);
        assert!(lazy.is_loaded());
        assert_eq!(builds.load(Ordering::SeqCst), 1);

        // Built once, not per call.
        lazy.embed(&["again".to_string()]).await.unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    /// A failed build is not cached: a cold model cache with no network
    /// should not disable search for the life of the process.
    #[tokio::test]
    async fn a_failed_build_is_retried_not_remembered() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let lazy = LazyEmbedder::new("test", 8, move || {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err(crate::RagError::Embedder("no network".into()))
            } else {
                Ok(Arc::new(HashEmbedder::new(8)) as Arc<dyn Embedder>)
            }
        });

        assert!(lazy.embed(&["x".to_string()]).await.is_err());
        assert!(!lazy.is_loaded());
        // Second attempt succeeds, so the first failure was not sticky.
        assert!(lazy.embed(&["x".to_string()]).await.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
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
            // On a cold cache this fetches ~120 MB from HuggingFace. With
            // progress suppressed the tool just appeared to hang on first use,
            // with no output and no explanation — which reads as a freeze, and
            // is a poor look for a tool that advertises a local-only mode.
            // Announce it, and let fastembed draw the progress bar.
            // "Cached" = the cache directory exists and has something in it.
            // Matching an exact model directory name is brittle: fastembed's
            // layout is its own business and changes between versions, and a
            // stale guess here means the banner prints on every single run.
            let already_cached = cache_dir
                .as_ref()
                .and_then(|d| std::fs::read_dir(d).ok())
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false);
            if !already_cached {
                eprintln!(
                    "wingman: downloading the embedding model (~120 MB, one time) \
                     for semantic search…"
                );
            }
            let mut opts = InitOptions::new(EmbeddingModel::BGESmallENV15)
                .with_show_download_progress(!already_cached);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if na == 0.0 || nb == 0.0 {
            0.0
        } else {
            dot / (na * nb)
        }
    }

    async fn embed_one(e: &HashEmbedder, s: &str) -> Vec<f32> {
        e.embed(&[s.to_string()]).await.unwrap().pop().unwrap()
    }

    #[test]
    fn dim_has_a_floor() {
        assert_eq!(HashEmbedder::new(2).dim(), 8); // floored to 8
        assert_eq!(HashEmbedder::new(128).dim(), 128);
        assert_eq!(HashEmbedder::default().dim(), 64);
    }

    #[tokio::test]
    async fn output_matches_batch_length_and_dim() {
        let e = HashEmbedder::new(32);
        let out = e
            .embed(&["a".into(), "b".into(), "c".into()])
            .await
            .unwrap();
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|v| v.len() == 32));
    }

    #[tokio::test]
    async fn deterministic_and_l2_normalized() {
        let e = HashEmbedder::new(64);
        let a = embed_one(&e, "the quick brown fox").await;
        let b = embed_one(&e, "the quick brown fox").await;
        assert_eq!(a, b, "same text must embed identically");
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "vector must be L2-normalized, got {norm}"
        );
    }

    #[tokio::test]
    async fn empty_text_is_zero_vector() {
        let e = HashEmbedder::new(16);
        let v = embed_one(&e, "   ").await;
        assert!(v.iter().all(|&x| x == 0.0));
    }

    #[tokio::test]
    async fn token_overlap_raises_similarity() {
        // A larger dim keeps hash collisions rare so overlap dominates.
        let e = HashEmbedder::new(512);
        let base = embed_one(&e, "database connection pool timeout").await;
        let similar = embed_one(&e, "database connection pool retry").await;
        let different = embed_one(&e, "banana smoothie recipe kitchen").await;
        let sim_overlap = cosine(&base, &similar);
        let sim_none = cosine(&base, &different);
        assert!(
            sim_overlap > sim_none,
            "shared tokens should score higher: overlap={sim_overlap} none={sim_none}"
        );
    }

    #[tokio::test]
    async fn case_insensitive_tokenization() {
        let e = HashEmbedder::new(128);
        let lower = embed_one(&e, "hello world").await;
        let upper = embed_one(&e, "HELLO WORLD").await;
        assert_eq!(lower, upper);
    }
}
