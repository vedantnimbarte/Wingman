//! Index finished sessions into a RAG store so the agent can recall
//! "have we discussed this before?" across runs and projects.
//!
//! Reuses [`arccode_rag::IndexStore`] but uses a synthetic path of the form
//! `session:<session_id>` so semantic-search results over code and over
//! conversations can share the same store without colliding.

use std::path::PathBuf;
use std::sync::Arc;

use arccode_rag::{Chunk, Embedder, IndexStore, ScoredChunk};
use arccode_session::{load_session, SessionRecord};

use crate::Result;

/// Build a per-user, cross-project session store at `~/.arccode/sessions.db`.
pub fn open_global_store(embedder: &dyn Embedder) -> Result<Arc<IndexStore>> {
    let dir = arccode_config::ensure_global_dir()?;
    let path = dir.join("sessions.db");
    let store = IndexStore::open(&path, embedder.id(), embedder.dim())
        .map_err(|e| crate::LearnError::Other(format!("could not open sessions.db: {e}")))?;
    Ok(Arc::new(store))
}

/// Read `session_path` and produce coarse chunks suitable for embedding.
///
/// A "thread chunk" is one user prompt + the assistant text/tool text that
/// followed it, capped at `cap_chars`. We don't try to be precious about
/// tool result content — the goal is recall over "what was the topic" not
/// reproducing the exact bytes.
pub fn chunk_session(session_path: &std::path::Path, cap_chars: usize) -> Result<Vec<Chunk>> {
    let session_id = session_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session")
        .to_string();

    let records = load_session(session_path).map_err(|e| {
        crate::LearnError::Other(format!("read session {}: {e}", session_path.display()))
    })?;

    let chunk_path = format!("session:{session_id}");
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut current = String::new();
    let mut line_start: u32 = 1;
    let mut line_cursor: u32 = 1;

    let flush = |buf: &mut String,
                 start: &mut u32,
                 cursor: &mut u32,
                 chunks: &mut Vec<Chunk>,
                 path: &str| {
        let body = buf.trim().to_string();
        if !body.is_empty() {
            chunks.push(Chunk {
                path: path.to_string(),
                start_line: *start,
                end_line: (*cursor).max(*start),
                content: body,
                symbol: None,
            });
        }
        buf.clear();
        *start = *cursor;
    };

    for rec in &records {
        let (label, body) = match rec {
            SessionRecord::User { text, .. } => ("USER", text.clone()),
            SessionRecord::Assistant { blocks, .. } => {
                let mut s = String::new();
                for b in blocks {
                    match b {
                        arccode_core::ContentBlock::Text { text } => {
                            if !s.is_empty() {
                                s.push('\n');
                            }
                            s.push_str(text);
                        }
                        arccode_core::ContentBlock::ToolUse { name, .. } => {
                            if !s.is_empty() {
                                s.push('\n');
                            }
                            s.push_str(&format!("[tool: {name}]"));
                        }
                        _ => {}
                    }
                }
                ("ASSIST", s)
            }
            SessionRecord::ToolResult { output, .. } => ("TOOL", truncate(output, 200)),
            _ => continue,
        };
        if body.trim().is_empty() {
            continue;
        }
        // Scrub credentials before anything reaches the embedding store —
        // sessions outlive transcripts once embedded into sessions.db.
        let body = crate::redact::redact_secrets(&body);
        let entry = format!("{label}: {}\n", body.trim());
        line_cursor = line_cursor.saturating_add(entry.matches('\n').count() as u32);

        // If a new user prompt arrives and the buffer is already big enough,
        // start a new chunk so chunks roughly align to threads of work.
        let is_new_prompt = matches!(rec, SessionRecord::User { .. });
        if is_new_prompt && current.len() >= cap_chars / 2 {
            flush(
                &mut current,
                &mut line_start,
                &mut line_cursor,
                &mut chunks,
                &chunk_path,
            );
        }
        current.push_str(&entry);
        if current.len() >= cap_chars {
            flush(
                &mut current,
                &mut line_start,
                &mut line_cursor,
                &mut chunks,
                &chunk_path,
            );
        }
    }
    if !current.trim().is_empty() {
        flush(
            &mut current,
            &mut line_start,
            &mut line_cursor,
            &mut chunks,
            &chunk_path,
        );
    }
    Ok(chunks)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Embed and write the chunks to `store`. Idempotent per session because
/// the store keys chunks by `path` and we use `session:<id>`.
pub async fn index_session_into(
    store: &IndexStore,
    embedder: &dyn Embedder,
    session_path: &std::path::Path,
) -> Result<usize> {
    let chunks = chunk_session(session_path, 1500)?;
    if chunks.is_empty() {
        return Ok(0);
    }
    let path_key = chunks[0].path.clone();
    let bodies: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let embeddings = embedder
        .embed(&bodies)
        .await
        .map_err(|e| crate::LearnError::Other(format!("embed: {e}")))?;
    // file_hash isn't meaningful for sessions, so we use the session id.
    let fake_hash = path_key.clone();
    store
        .replace_file(&path_key, &fake_hash, &chunks, &embeddings)
        .map_err(|e| crate::LearnError::Other(format!("store: {e}")))?;
    Ok(chunks.len())
}

/// Search a session store and return hits with their session id parsed out
/// of the synthetic path.
pub async fn search_sessions(
    store: &IndexStore,
    embedder: &dyn Embedder,
    query: &str,
    limit: usize,
) -> Result<Vec<SessionHit>> {
    let q_str = vec![query.to_string()];
    let embeds = embedder
        .embed(&q_str)
        .await
        .map_err(|e| crate::LearnError::Other(format!("embed: {e}")))?;
    let q = embeds
        .into_iter()
        .next()
        .ok_or_else(|| crate::LearnError::Other("embedder returned no vector".into()))?;
    let raw = store
        .search(&q, limit)
        .map_err(|e| crate::LearnError::Other(format!("search: {e}")))?;
    Ok(raw.into_iter().map(SessionHit::from).collect())
}

#[derive(Debug, Clone)]
pub struct SessionHit {
    pub session_id: String,
    pub snippet: String,
    pub score: f32,
}

impl From<ScoredChunk> for SessionHit {
    fn from(c: ScoredChunk) -> Self {
        let session_id = c
            .path
            .strip_prefix("session:")
            .unwrap_or(&c.path)
            .to_string();
        Self {
            session_id,
            snippet: c.content,
            score: c.score,
        }
    }
}

/// Locate the on-disk session JSONL for `session_id` by walking both the
/// per-project sessions dir and any other project's sessions you happen to
/// know about. Currently we only check the project-local dir; cross-project
/// retrieval requires the caller to maintain its own session-id-to-path map.
pub fn session_path_for(project_root: &std::path::Path, session_id: &str) -> Option<PathBuf> {
    let dir = project_root.join(".arccode").join("sessions");
    let candidate = dir.join(format!("{session_id}.jsonl"));
    if candidate.exists() {
        return Some(candidate);
    }
    None
}

/// Walk the project's sessions dir and embed any sessions that aren't yet
/// in `store`. Useful at startup to backfill the index without needing to
/// hook session shutdown. Returns the number of sessions indexed.
pub async fn backfill_project_sessions(
    project_root: &std::path::Path,
    store: &IndexStore,
    embedder: &dyn Embedder,
) -> Result<usize> {
    let sessions_dir = project_root.join(".arccode").join("sessions");
    if !sessions_dir.exists() {
        return Ok(0);
    }
    let mut indexed = 0usize;
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(_) => return Ok(0),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let session_id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let key = format!("session:{session_id}");
        // Skip if the store already has chunks under this key.
        if store
            .file_hash(&key)
            .map_err(|e| crate::LearnError::Other(format!("file_hash: {e}")))?
            .is_some()
        {
            continue;
        }
        match index_session_into(store, embedder, &path).await {
            Ok(n) if n > 0 => indexed += 1,
            Ok(_) => {}
            Err(e) => tracing::warn!("backfill skip {}: {e}", path.display()),
        }
    }
    Ok(indexed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arccode_rag::HashEmbedder;
    use std::io::Write;

    fn write_session(path: &std::path::Path) {
        let mut f = std::fs::File::create(path).unwrap();
        let lines = [
            r#"{"kind":"session_start","ts":"now","model":"m","provider":"p","system_hash":null}"#,
            r#"{"kind":"user","ts":"now","text":"how does the cache work in the loop?"}"#,
            r#"{"kind":"assistant","ts":"now","blocks":[{"type":"text","text":"The agent caches per turn..."}]}"#,
            r#"{"kind":"user","ts":"now","text":"thanks, can we also disable it?"}"#,
            r#"{"kind":"assistant","ts":"now","blocks":[{"type":"text","text":"Yes, clear tool_cache in run()."}]}"#,
            r#"{"kind":"stop","ts":"now","reason":"\"end_turn\""}"#,
        ];
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn chunks_session_into_at_least_one_chunk() {
        let dir = std::env::temp_dir().join(format!(
            "arccode-learn-si-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("20260101T000000000Z.jsonl");
        write_session(&session);
        let chunks = chunk_session(&session, 1500).unwrap();
        assert!(!chunks.is_empty());
        assert!(chunks[0].path.starts_with("session:"));
        assert!(chunks[0].content.contains("USER:"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn end_to_end_index_and_search() {
        let dir = std::env::temp_dir().join(format!(
            "arccode-learn-si2-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("20260101T010101000Z.jsonl");
        write_session(&session);

        let embedder = HashEmbedder::default();
        let store_path = dir.join("sessions.db");
        let store = IndexStore::open(&store_path, embedder.id(), embedder.dim()).unwrap();

        let n = index_session_into(&store, &embedder, &session)
            .await
            .unwrap();
        assert!(n >= 1);

        let hits = search_sessions(&store, &embedder, "cache disable loop", 5)
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].session_id.contains("20260101"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
