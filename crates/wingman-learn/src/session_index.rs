//! Index finished sessions into a RAG store so the agent can recall
//! "have we discussed this before?" across runs and projects.
//!
//! Reuses [`wingman_rag::IndexStore`] but uses a synthetic path of the form
//! `session:<session_id>` so semantic-search results over code and over
//! conversations can share the same store without colliding.

use std::path::PathBuf;
use std::sync::Arc;

use wingman_rag::{Chunk, Embedder, IndexStore, ScoredChunk};
use wingman_session::{load_session, SessionRecord};

use crate::Result;

/// Build a per-user, cross-project session store at `~/.wingman/sessions.db`.
pub fn open_global_store(embedder: &dyn Embedder) -> Result<Arc<IndexStore>> {
    let dir = wingman_config::ensure_global_dir()?;
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
                        wingman_core::ContentBlock::Text { text } => {
                            if !s.is_empty() {
                                s.push('\n');
                            }
                            s.push_str(text);
                        }
                        wingman_core::ContentBlock::ToolUse { name, .. } => {
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

/// Forget everything indexed for `session_id` in the global session store.
///
/// Deleting a session's transcript is not a delete while its chunks are still
/// in `~/.wingman/sessions.db`: `recall_session` would keep surfacing the
/// content of a conversation the user believes is gone. Whoever removes the
/// JSONL should call this too.
///
/// Opens the store for maintenance rather than with a real embedder, so this
/// costs a SQLite `DELETE` instead of loading an embedding model. `Ok(false)`
/// means there was no index to clean — not a failure.
pub fn forget_session(session_id: &str) -> Result<bool> {
    let dir = wingman_config::global_dir()?;
    let path = dir.join("sessions.db");
    let store = IndexStore::open_for_maintenance(&path)
        .map_err(|e| crate::LearnError::Other(format!("open sessions.db: {e}")))?;
    let Some(store) = store else {
        return Ok(false);
    };
    store
        .forget(&format!("session:{session_id}"))
        .map_err(|e| crate::LearnError::Other(format!("forget session: {e}")))?;
    Ok(true)
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
    // `session_id` arrives from the model, so it is untrusted. `Path::join`
    // with an absolute path silently replaces the base, and `..` walks out of
    // the sessions directory — either would turn this into an arbitrary
    // `*.jsonl` read (session transcripts contain whole conversations and all
    // tool output). Accept only a bare file-stem.
    if !is_safe_session_id(session_id) {
        return None;
    }
    let dir = project_root.join(".wingman").join("sessions");
    let candidate = dir.join(format!("{session_id}.jsonl"));
    if candidate.exists() {
        return Some(candidate);
    }
    None
}

/// A session id must be a single path component with no traversal, no
/// separators, and no drive/UNC prefix. Session ids Wingman generates look
/// like `2026-01-01-1200`, so this is deliberately strict.
fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.contains("..")
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains(':')
        && !id.contains('\0')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Backfill every unindexed session in the project, with no cutoff.
pub async fn backfill_project_sessions(
    project_root: &std::path::Path,
    store: &IndexStore,
    embedder: &dyn Embedder,
) -> Result<usize> {
    backfill_project_sessions_since(project_root, store, embedder, None).await
}

/// Queue of sessions waiting to be embedded, one absolute path per line.
///
/// Indexing a session costs an embedding-model load, which is far too much to
/// put on a process's exit path (see the comment in `commands/headless.rs`).
/// Recording *that there is work to do* costs a file append, so exit does that
/// and the next run does the embedding.
///
/// This is what makes the backfill global rather than per-project. Scanning
/// the current project at startup can never reach a session written in a repo
/// you have not opened since — which is precisely the session cross-project
/// recall exists to surface.
fn pending_path() -> Result<PathBuf> {
    Ok(wingman_config::ensure_global_dir()?.join("pending-sessions.txt"))
}

/// Note that `session_path` still needs indexing. Cheap enough for an exit
/// path: one append, no embedding, no database.
pub fn enqueue_pending(session_path: &std::path::Path) -> Result<()> {
    enqueue_pending_at(&pending_path()?, session_path)
}

/// [`enqueue_pending`] against an explicit queue file.
pub fn enqueue_pending_at(queue: &std::path::Path, session_path: &std::path::Path) -> Result<()> {
    use std::io::Write;
    let path = queue.to_path_buf();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| crate::LearnError::Other(format!("open {}: {e}", path.display())))?;
    writeln!(f, "{}", session_path.display())
        .map_err(|e| crate::LearnError::Other(format!("write {}: {e}", path.display())))?;
    Ok(())
}

/// Index everything on the queue, up to `limit` sessions, and clear it.
///
/// The queue is claimed by renaming it aside before any work starts, so two
/// wingman processes starting at once do not both embed the same backlog and
/// do not race each other's rewrite. Entries that are already indexed are
/// skipped cheaply; entries whose file has since been deleted are dropped;
/// anything left over — because it failed, or because `limit` was reached — is
/// put back for the next run.
///
/// `limit` keeps the first run after a long gap from embedding an unbounded
/// backlog before the user gets their prompt back.
pub async fn drain_pending(
    store: &IndexStore,
    embedder: &dyn Embedder,
    limit: usize,
) -> Result<usize> {
    drain_pending_at(&pending_path()?, store, embedder, limit).await
}

/// [`drain_pending`] against an explicit queue file.
pub async fn drain_pending_at(
    queue: &std::path::Path,
    store: &IndexStore,
    embedder: &dyn Embedder,
    limit: usize,
) -> Result<usize> {
    let path = queue.to_path_buf();
    if !path.exists() {
        return Ok(0);
    }
    let claimed = path.with_extension("txt.claimed");
    // Rename is the claim. Losing the race means another process owns this
    // batch, which is a reason to do nothing rather than to duplicate it.
    if std::fs::rename(&path, &claimed).is_err() {
        return Ok(0);
    }
    let body = std::fs::read_to_string(&claimed).unwrap_or_default();

    let mut indexed = 0usize;
    let mut leftover: Vec<String> = Vec::new();
    for (n, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if n >= limit {
            leftover.push(line.to_string());
            continue;
        }
        let session = std::path::Path::new(line);
        if !session.exists() {
            continue; // deleted since it was queued — nothing to index
        }
        let Some(session_id) = session.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let key = format!("session:{session_id}");
        if matches!(store.file_hash(&key), Ok(Some(_))) {
            continue; // already indexed, by a previous drain or the TUI
        }
        match index_session_into(store, embedder, session).await {
            Ok(n) if n > 0 => indexed += 1,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("pending session {line}: {e}");
                leftover.push(line.to_string());
            }
        }
    }

    let _ = std::fs::remove_file(&claimed);
    for line in leftover {
        let _ = enqueue_pending_at(&path, std::path::Path::new(&line));
    }
    Ok(indexed)
}

/// Walk the project's sessions dir and embed any sessions that aren't yet
/// in `store`. Useful at startup to backfill the index without needing to
/// hook session shutdown. Returns the number of sessions indexed.
/// `newer_than` excludes sessions that appeared after the caller started —
/// they belong to a live process (usually this one), which is still appending
/// to them and will queue them on exit via [`enqueue_pending`]. Without that
/// cutoff the scan embeds a half-written transcript and then gets killed when
/// the process exits, which is both wasted work and an alarming warning about
/// a session that was never in danger.
pub async fn backfill_project_sessions_since(
    project_root: &std::path::Path,
    store: &IndexStore,
    embedder: &dyn Embedder,
    newer_than: Option<std::time::SystemTime>,
) -> Result<usize> {
    let sessions_dir = project_root.join(".wingman").join("sessions");
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
        if let Some(cutoff) = newer_than {
            let live = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|m| m >= cutoff)
                .unwrap_or(false);
            if live {
                continue; // a running process owns this one
            }
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
    use std::io::Write;
    use wingman_rag::HashEmbedder;

    #[test]
    fn session_id_rejects_traversal_and_absolute_paths() {
        for bad in [
            "../../../etc/passwd",
            "..\\..\\Windows\\System32\\config",
            "/etc/shadow",
            "C:/Users/victim/.ssh/id_rsa",
            "\\\\server\\share\\x",
            "a/b",
            "a\\b",
            "..",
            "",
            "has space",
            "semi;colon",
        ] {
            assert!(
                !is_safe_session_id(bad),
                "should have rejected session id {bad:?}"
            );
        }
    }

    #[test]
    fn session_id_accepts_generated_shapes() {
        for good in ["2026-01-01-1200", "abc_123", "session.1", "A-b_C.9"] {
            assert!(
                is_safe_session_id(good),
                "should have accepted session id {good:?}"
            );
        }
    }

    #[test]
    fn traversal_id_resolves_to_none_even_if_target_exists() {
        let dir = std::env::temp_dir().join("wingman-sessid-test");
        let _ = std::fs::create_dir_all(&dir);
        let outside = dir.join("outside.jsonl");
        let _ = std::fs::write(&outside, "{}");

        let root = dir.join("proj");
        let _ = std::fs::create_dir_all(root.join(".wingman").join("sessions"));

        // Would previously escape to ../outside.jsonl.
        assert!(session_path_for(&root, "../outside").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A temp dir per test — the queue must never be the user's real
    /// `~/.wingman/pending-sessions.txt`.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wingman-pending-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store_in(dir: &std::path::Path, emb: &HashEmbedder) -> IndexStore {
        IndexStore::open(&dir.join("sessions.db"), emb.id(), emb.dim()).unwrap()
    }

    #[tokio::test]
    async fn a_queued_session_is_indexed_by_the_next_drain() {
        let dir = scratch("basic");
        let emb = HashEmbedder::default();
        let store = store_in(&dir, &emb);
        let session = dir.join("20260101T000000000Z.jsonl");
        write_session(&session);
        let queue = dir.join("pending.txt");

        enqueue_pending_at(&queue, &session).unwrap();
        assert_eq!(drain_pending_at(&queue, &store, &emb, 25).await.unwrap(), 1);

        // Recallable now, and the queue is emptied rather than replayed.
        assert!(store
            .file_hash("session:20260101T000000000Z")
            .unwrap()
            .is_some());
        assert!(!queue.exists());
        assert_eq!(drain_pending_at(&queue, &store, &emb, 25).await.unwrap(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_limit_bounds_one_run_and_requeues_the_rest() {
        let dir = scratch("limit");
        let emb = HashEmbedder::default();
        let store = store_in(&dir, &emb);
        let queue = dir.join("pending.txt");
        for i in 0..4 {
            let s = dir.join(format!("2026010{i}T000000000Z.jsonl"));
            write_session(&s);
            enqueue_pending_at(&queue, &s).unwrap();
        }

        // A long gap must not turn the next launch into a batch job.
        assert_eq!(drain_pending_at(&queue, &store, &emb, 2).await.unwrap(), 2);
        // The remainder is not lost — it waits for the run after.
        assert_eq!(drain_pending_at(&queue, &store, &emb, 25).await.unwrap(), 2);
        assert_eq!(drain_pending_at(&queue, &store, &emb, 25).await.unwrap(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn already_indexed_and_deleted_entries_are_dropped_quietly() {
        let dir = scratch("skip");
        let emb = HashEmbedder::default();
        let store = store_in(&dir, &emb);
        let queue = dir.join("pending.txt");

        let indexed = dir.join("20260101T000000001Z.jsonl");
        write_session(&indexed);
        index_session_into(&store, &emb, &indexed).await.unwrap();

        let gone = dir.join("20260101T000000002Z.jsonl");
        write_session(&gone);
        std::fs::remove_file(&gone).unwrap();

        enqueue_pending_at(&queue, &indexed).unwrap();
        enqueue_pending_at(&queue, &gone).unwrap();

        // Neither is work: one is done, the other no longer exists.
        assert_eq!(drain_pending_at(&queue, &store, &emb, 25).await.unwrap(), 0);
        assert!(!queue.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn draining_a_queue_that_does_not_exist_is_not_an_error() {
        let dir = scratch("empty");
        let emb = HashEmbedder::default();
        let store = store_in(&dir, &emb);
        assert_eq!(
            drain_pending_at(&dir.join("nope.txt"), &store, &emb, 25)
                .await
                .unwrap(),
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
            "wingman-learn-si-{}-{}",
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
            "wingman-learn-si2-{}-{}",
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
