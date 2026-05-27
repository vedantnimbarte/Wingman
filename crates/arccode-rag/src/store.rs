//! SQLite-backed index store.
//!
//! Schema (single table):
//!
//! ```text
//! chunks(
//!   id          INTEGER PRIMARY KEY,
//!   path        TEXT     NOT NULL,
//!   start_line  INTEGER  NOT NULL,
//!   end_line    INTEGER  NOT NULL,
//!   content     TEXT     NOT NULL,
//!   file_hash   TEXT     NOT NULL,
//!   embedding   BLOB     NOT NULL    -- Vec<f32> laid out as bytes
//! )
//! ```
//!
//! `meta(key, value)` records the embedder id + dim so we can refuse to
//! mix vectors from different models.
//!
//! Search is **in-memory cosine**: we load all embeddings once, sort, and
//! return the top N. This is fast (<10 ms) up to ~50k chunks; beyond that
//! we'll wire `sqlite-vec` as an extension.

use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::chunker::Chunk;
use crate::{RagError, Result};

pub struct IndexStore {
    db: Mutex<Connection>,
    path: PathBuf,
    embedder_id: String,
    dim: usize,
}

#[derive(Debug, Clone)]
pub struct ScoredChunk {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub content: String,
    pub score: f32,
}

impl IndexStore {
    /// Open or create the index file at `db_path`. The embedder id and dim
    /// are recorded on first open; subsequent opens against a different
    /// embedder return [`RagError::DimMismatch`].
    pub fn open(db_path: &Path, embedder_id: &str, dim: usize) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path)?;
        Self::init_schema(&conn)?;

        let existing_id: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'embedder_id'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let existing_dim: Option<i64> = conn
            .query_row("SELECT value FROM meta WHERE key = 'dim'", [], |r| {
                r.get::<_, String>(0)
                    .map(|s| s.parse::<i64>().unwrap_or(0))
            })
            .optional()?;

        match (existing_id.as_deref(), existing_dim) {
            (Some(id), Some(d)) if id != embedder_id || d as usize != dim => {
                return Err(RagError::DimMismatch {
                    expected: d as usize,
                    actual: dim,
                });
            }
            (None, _) | (_, None) => {
                conn.execute(
                    "INSERT OR REPLACE INTO meta(key, value) VALUES ('embedder_id', ?1)",
                    params![embedder_id],
                )?;
                conn.execute(
                    "INSERT OR REPLACE INTO meta(key, value) VALUES ('dim', ?1)",
                    params![dim.to_string()],
                )?;
            }
            _ => {}
        }

        Ok(Self {
            db: Mutex::new(conn),
            path: db_path.to_path_buf(),
            embedder_id: embedder_id.to_string(),
            dim,
        })
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS chunks (
                id         INTEGER PRIMARY KEY,
                path       TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line   INTEGER NOT NULL,
                content    TEXT NOT NULL,
                file_hash  TEXT NOT NULL,
                embedding  BLOB NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_chunks_path ON chunks(path);
             CREATE INDEX IF NOT EXISTS idx_chunks_hash ON chunks(file_hash);
            ",
        )?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn embedder_id(&self) -> &str {
        &self.embedder_id
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Return the file hash currently stored for `rel_path`, if any.
    pub fn file_hash(&self, rel_path: &str) -> Result<Option<String>> {
        let conn = self.db.lock().unwrap();
        let h: Option<String> = conn
            .query_row(
                "SELECT file_hash FROM chunks WHERE path = ?1 LIMIT 1",
                params![rel_path],
                |r| r.get(0),
            )
            .optional()?;
        Ok(h)
    }

    /// Replace all chunks for a path with `chunks` + their `embeddings`.
    pub fn replace_file(
        &self,
        rel_path: &str,
        file_hash: &str,
        chunks: &[Chunk],
        embeddings: &[Vec<f32>],
    ) -> Result<()> {
        if chunks.len() != embeddings.len() {
            return Err(RagError::Other(format!(
                "chunks/embeddings mismatch: {} vs {}",
                chunks.len(),
                embeddings.len()
            )));
        }
        for emb in embeddings {
            if emb.len() != self.dim {
                return Err(RagError::DimMismatch {
                    expected: self.dim,
                    actual: emb.len(),
                });
            }
        }
        let mut conn = self.db.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM chunks WHERE path = ?1", params![rel_path])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO chunks(path, start_line, end_line, content, file_hash, embedding) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (c, emb) in chunks.iter().zip(embeddings.iter()) {
                stmt.execute(params![
                    c.path,
                    c.start_line as i64,
                    c.end_line as i64,
                    c.content,
                    file_hash,
                    f32_slice_to_bytes(emb),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete every chunk under `rel_path`.
    pub fn forget(&self, rel_path: &str) -> Result<()> {
        let conn = self.db.lock().unwrap();
        conn.execute("DELETE FROM chunks WHERE path = ?1", params![rel_path])?;
        Ok(())
    }

    /// Count of indexed chunks.
    pub fn chunk_count(&self) -> Result<u64> {
        let conn = self.db.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Cosine similarity search against `query`. Returns up to `limit`
    /// chunks ordered by descending score.
    pub fn search(&self, query: &[f32], limit: usize) -> Result<Vec<ScoredChunk>> {
        if query.len() != self.dim {
            return Err(RagError::DimMismatch {
                expected: self.dim,
                actual: query.len(),
            });
        }
        let q_norm = norm(query);
        if q_norm == 0.0 {
            return Ok(Vec::new());
        }
        let conn = self.db.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT path, start_line, end_line, content, embedding FROM chunks")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u32,
                r.get::<_, i64>(2)? as u32,
                r.get::<_, String>(3)?,
                r.get::<_, Vec<u8>>(4)?,
            ))
        })?;

        let mut scored: Vec<ScoredChunk> = Vec::new();
        for row in rows {
            let (path, start_line, end_line, content, emb_bytes) = row?;
            let emb = bytes_to_f32_vec(&emb_bytes);
            if emb.len() != self.dim {
                continue;
            }
            let dot: f32 = query.iter().zip(emb.iter()).map(|(a, b)| a * b).sum();
            let e_norm = norm(&emb);
            if e_norm == 0.0 {
                continue;
            }
            let score = dot / (q_norm * e_norm);
            scored.push(ScoredChunk {
                path,
                start_line,
                end_line,
                content,
                score,
            });
        }
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(limit);
        Ok(scored)
    }
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

fn f32_slice_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn bytes_to_f32_vec(b: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(b.len() / 4);
    for chunk in b.chunks_exact(4) {
        let arr: [u8; 4] = chunk.try_into().unwrap();
        out.push(f32::from_le_bytes(arr));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db() -> PathBuf {
        std::env::temp_dir().join(format!(
            "arccode-store-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn write_then_search_finds_best_chunk() {
        let path = tmp_db();
        let store = IndexStore::open(&path, "test", 4).unwrap();
        let chunks = [
            Chunk {
                path: "a.rs".into(),
                start_line: 1,
                end_line: 10,
                content: "alpha".into(),
            },
            Chunk {
                path: "b.rs".into(),
                start_line: 1,
                end_line: 10,
                content: "beta".into(),
            },
        ];
        let embeddings = [vec![1.0, 0.0, 0.0, 0.0], vec![0.0, 1.0, 0.0, 0.0]];
        store
            .replace_file("a.rs", "h1", &chunks[..1], &embeddings[..1])
            .unwrap();
        store
            .replace_file("b.rs", "h2", &chunks[1..], &embeddings[1..])
            .unwrap();

        let results = store.search(&[0.9, 0.1, 0.0, 0.0], 5).unwrap();
        assert_eq!(results[0].path, "a.rs");
        assert!(results[0].score > results[1].score);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replace_file_overwrites_previous_chunks() {
        let path = tmp_db();
        let store = IndexStore::open(&path, "test", 2).unwrap();
        let make = |c: &str| Chunk {
            path: "x.rs".into(),
            start_line: 1,
            end_line: 1,
            content: c.into(),
        };
        store
            .replace_file("x.rs", "h1", &[make("v1")], &[vec![1.0, 0.0]])
            .unwrap();
        assert_eq!(store.chunk_count().unwrap(), 1);
        store
            .replace_file(
                "x.rs",
                "h2",
                &[make("v2"), make("v2b")],
                &[vec![0.0, 1.0], vec![1.0, 1.0]],
            )
            .unwrap();
        assert_eq!(store.chunk_count().unwrap(), 2);
        let _ = std::fs::remove_file(&path);
    }
}
