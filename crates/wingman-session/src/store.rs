//! The session-store seam.
//!
//! Sessions are JSONL files under `<project>/.wingman/sessions/`, and every
//! consumer reached them by path: `list_sessions(dir)` then
//! `load_session(path)`. That is one implementation shared by all callers,
//! which is fine until you want a second one.
//!
//! [`SessionStore`] is that seam. It addresses sessions by **id** rather than
//! path, which is what callers actually mean — the orchestrator records an id,
//! `wingman session fork` takes an id, `--resume` takes an id — and it leaves
//! where the bytes live to the implementation.
//!
//! Two implementations ship, so the interface is shaped by two real callers
//! rather than one imagined one:
//!
//! - [`FileSessionStore`] — the JSONL files, exactly as before.
//! - [`MemorySessionStore`] — no filesystem at all, for tests that care about
//!   session *content* rather than session *storage*. Those tests currently
//!   create temp directories and write real files to assert on parsing, which
//!   is slower and leaves litter when they fail.
//!
//! `records_to_messages` stays a free function: it is a pure projection from
//! records to messages and has nothing to do with where they came from.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::{list_sessions, load_session, SessionError, SessionRecord};

/// Somewhere sessions are kept.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Every record in one session, oldest first.
    ///
    /// Synchronous, because both implementations are: a file read and a map
    /// lookup. Making it async for symmetry with `append` would push `.await`
    /// — and an async context — onto every sync caller to buy nothing.
    fn load(&self, id: &str) -> Result<Vec<SessionRecord>, SessionError>;

    /// Known session ids, newest first.
    fn list(&self) -> Vec<String>;

    /// Append one record, creating the session if it does not exist.
    ///
    /// Async because the file store's write is: this is the one operation
    /// that actually touches an I/O runtime.
    async fn append(&self, id: &str, record: SessionRecord) -> Result<(), SessionError>;

    /// Whether a session exists. Default is a `load`, which implementations
    /// backed by something cheaper should override.
    fn exists(&self, id: &str) -> bool {
        self.load(id).is_ok()
    }
}

/// Sessions as JSONL files in a directory — the on-disk format.
#[derive(Debug, Clone)]
pub struct FileSessionStore {
    dir: PathBuf,
}

impl FileSessionStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file backing `id`, whether or not it exists.
    ///
    /// The id is validated rather than trusted: it reaches this from a command
    /// line, an HTTP request, and the model, and a `..` in it would otherwise
    /// place the file outside the sessions directory.
    fn path_for(&self, id: &str) -> Result<PathBuf, SessionError> {
        if !crate::is_valid_session_id(id) {
            return Err(SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid session id: {id}"),
            )));
        }
        Ok(self.dir.join(format!("{id}.jsonl")))
    }
}

#[async_trait]
impl SessionStore for FileSessionStore {
    fn load(&self, id: &str) -> Result<Vec<SessionRecord>, SessionError> {
        load_session(&self.path_for(id)?)
    }

    fn list(&self) -> Vec<String> {
        list_sessions(&self.dir)
            .into_iter()
            .filter_map(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
            })
            .collect()
    }

    async fn append(&self, id: &str, record: SessionRecord) -> Result<(), SessionError> {
        let mut log = crate::SessionLog::open_named(&self.dir, id).await?;
        log.write(record).await
    }

    fn exists(&self, id: &str) -> bool {
        self.path_for(id).map(|p| p.exists()).unwrap_or(false)
    }
}

/// Sessions held in memory, for tests and for anything that wants a session
/// that never touches disk.
#[derive(Debug, Default)]
pub struct MemorySessionStore {
    /// Insertion order is kept so `list` can return newest-first without the
    /// timestamp-shaped filenames the file store sorts by.
    sessions: Mutex<Vec<(String, Vec<SessionRecord>)>>,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a session outright. Convenient in tests that want a transcript to
    /// read rather than one to build up record by record.
    pub fn seed(&self, id: &str, records: Vec<SessionRecord>) {
        let mut all = self.sessions.lock().unwrap();
        match all.iter_mut().find(|(k, _)| k == id) {
            Some((_, existing)) => *existing = records,
            None => all.push((id.to_string(), records)),
        }
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    fn load(&self, id: &str) -> Result<Vec<SessionRecord>, SessionError> {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| {
                SessionError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("no such session: {id}"),
                ))
            })
    }

    fn list(&self) -> Vec<String> {
        let all = self.sessions.lock().unwrap();
        all.iter().rev().map(|(k, _)| k.clone()).collect()
    }

    async fn append(&self, id: &str, record: SessionRecord) -> Result<(), SessionError> {
        let mut all = self.sessions.lock().unwrap();
        match all.iter_mut().find(|(k, _)| k == id) {
            Some((_, records)) => records.push(record),
            None => all.push((id.to_string(), vec![record])),
        }
        Ok(())
    }

    fn exists(&self, id: &str) -> bool {
        self.sessions.lock().unwrap().iter().any(|(k, _)| k == id)
    }
}

/// Sessions keyed by id, as a plain map. Handy for a caller holding
/// transcripts it did not load from anywhere.
impl From<HashMap<String, Vec<SessionRecord>>> for MemorySessionStore {
    fn from(map: HashMap<String, Vec<SessionRecord>>) -> Self {
        let store = Self::new();
        for (id, records) in map {
            store.seed(&id, records);
        }
        store
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> SessionRecord {
        SessionRecord::User {
            ts: "t".into(),
            text: text.into(),
        }
    }

    /// Both implementations have to behave the same way, or the seam is a
    /// lie. Run the same script against each.
    async fn behaves_like_a_store(store: &dyn SessionStore) {
        assert!(!store.exists("s1"));
        assert!(store.load("s1").is_err(), "unknown session must error");

        store.append("s1", user("hello")).await.unwrap();
        store.append("s1", user("again")).await.unwrap();
        assert!(store.exists("s1"));

        let records = store.load("s1").unwrap();
        assert_eq!(records.len(), 2, "appends accumulate in order");
        match &records[0] {
            SessionRecord::User { text, .. } => assert_eq!(text, "hello"),
            other => panic!("wrong record: {other:?}"),
        }

        store.append("s2", user("other")).await.unwrap();
        let ids = store.list();
        assert!(ids.contains(&"s1".to_string()));
        assert!(ids.contains(&"s2".to_string()));
    }

    #[tokio::test]
    async fn the_memory_store_behaves_like_a_store() {
        behaves_like_a_store(&MemorySessionStore::new()).await;
    }

    #[tokio::test]
    async fn the_file_store_behaves_like_a_store() {
        let dir = std::env::temp_dir().join(format!(
            "wingman-store-seam-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        behaves_like_a_store(&FileSessionStore::new(&dir)).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The id reaches this from a command line, an HTTP request, and the
    /// model. A traversal must not place a file outside the sessions
    /// directory.
    #[tokio::test]
    async fn the_file_store_refuses_a_traversal_id() {
        let dir = std::env::temp_dir().join("wingman-store-traversal");
        let store = FileSessionStore::new(&dir);
        for bad in ["../../etc/passwd", "..\\..\\windows", "/abs", "a/b"] {
            assert!(store.load(bad).is_err(), "should refuse {bad}");
            assert!(!store.exists(bad), "should refuse {bad}");
            assert!(
                store.append(bad, user("x")).await.is_err(),
                "should refuse {bad}"
            );
        }
    }

    #[tokio::test]
    async fn seeding_replaces_rather_than_appends() {
        let store = MemorySessionStore::new();
        store.seed("s", vec![user("first")]);
        store.seed("s", vec![user("second")]);
        assert_eq!(store.load("s").unwrap().len(), 1);
    }

    /// A record is on disk by the time `append` returns.
    ///
    /// The store opens a fresh log per append and drops it, so anything left
    /// buffered lands after the *next* handle's write — which is how a line
    /// ends up torn rather than merely late.
    #[tokio::test]
    async fn a_record_is_on_disk_when_append_returns() {
        let dir = std::env::temp_dir().join(format!(
            "wingman-durable-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = FileSessionStore::new(&dir);

        store.append("s1", user("hello")).await.unwrap();
        let path = dir.join("s1.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.ends_with('\n'),
            "the newline must land with the record, not after the next one: {text:?}"
        );
        assert_eq!(text.lines().count(), 1, "{text:?}");

        store.append("s1", user("again")).await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2, "{text:?}");
        // The real symptom: two records concatenated onto one line parse as
        // "trailing characters".
        assert_eq!(store.load("s1").unwrap().len(), 2);
    }
}
