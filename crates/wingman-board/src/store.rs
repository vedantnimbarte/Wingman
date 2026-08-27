//! The board database: `~/.wingman/board.db`.
//!
//! Holds the one thing pilot runs cannot: durable identity. A card is a goal
//! you authored; it outlives the runs that execute it and spans projects.
//! Everything else the board shows — task status, agents, models, cost — is
//! read through `wingman_autonomous::dashboard` from the run's own
//! `state.json`, which stays the single source of truth.
//!
//! Follows the `wingman-learn` `StatsStore` precedent (`Connection` behind a
//! `Mutex`, `CREATE TABLE IF NOT EXISTS`). It opens through
//! `wingman_rag::sqlite`, which sets WAL and a busy timeout because the board
//! TUI is a long-lived reader while `pilot` commands write the registry
//! concurrently. This store needed those first; they now live in one place, so
//! `learn.db` and the index databases — which have the same problem — get them
//! too.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;

/// Current schema version. Bump when adding a migration step below.
pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum BoardError {
    #[error("board database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Config(#[from] wingman_config::ConfigError),
    #[error("no card matches `{0}`")]
    NoSuchCard(String),
    #[error("`{prefix}` is ambiguous: {}", .candidates.join(", "))]
    AmbiguousCard {
        prefix: String,
        candidates: Vec<String>,
    },
    #[error("no project matches `{0}`")]
    NoSuchProject(String),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, BoardError>;

/// Open handle to `board.db`.
pub struct BoardStore {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl BoardStore {
    /// Open or create `~/.wingman/board.db`.
    pub fn open_default() -> Result<Self> {
        let dir = wingman_config::ensure_global_dir()?;
        Self::open(&dir.join("board.db"))
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).map_err(|source| BoardError::Io {
                path: p.to_path_buf(),
                source,
            })?;
        }
        // Shared open: WAL lets the TUI read while a `pilot` command writes
        // the registry. This store had these pragmas first; they now live in
        // one place so the other databases get them too.
        let conn = wingman_rag::sqlite::open(path)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        let store = Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run whatever forward-only migration steps the stored version needs.
    /// At version 1 there is nothing to do but record the version.
    fn migrate(&self) -> Result<()> {
        let from = self.schema_version()?;
        if from == SCHEMA_VERSION {
            return Ok(());
        }
        // Future steps go here, guarded by `from`.
        self.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .get_meta("schema_version")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
        let mut rows = stmt.query([key])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (key, value),
        )?;
        Ok(())
    }

    /// A poisoned mutex means another thread panicked mid-statement. The
    /// connection itself is still usable — SQLite is transactional — so
    /// recovering the guard beats propagating a panic into the TUI.
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS project (
    id         TEXT PRIMARY KEY,
    root       TEXT NOT NULL UNIQUE,
    name       TEXT NOT NULL,
    last_seen  TEXT NOT NULL,
    hidden     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS card (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES project(id),
    title       TEXT NOT NULL,
    goal        TEXT NOT NULL DEFAULT '',
    notes       TEXT,
    labels      TEXT NOT NULL DEFAULT '',
    ord         REAL NOT NULL,
    archived    INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_card_project ON card(project_id, archived);

CREATE TABLE IF NOT EXISTS dispatch (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    card_id     TEXT NOT NULL REFERENCES card(id) ON DELETE CASCADE,
    project_id  TEXT NOT NULL,
    run_id      TEXT NOT NULL,
    run_dir     TEXT NOT NULL,
    started_at  TEXT NOT NULL,
    ended_at    TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_dispatch_run ON dispatch(project_id, run_id);
CREATE INDEX IF NOT EXISTS idx_dispatch_card ON dispatch(card_id);

CREATE TABLE IF NOT EXISTS rollup (
    run_dir   TEXT PRIMARY KEY,
    mtime_ns  INTEGER NOT NULL,
    status    TEXT NOT NULL,
    done      INTEGER NOT NULL,
    total     INTEGER NOT NULL,
    failed    INTEGER NOT NULL,
    blocked   INTEGER NOT NULL,
    review    INTEGER NOT NULL,
    usd       REAL NOT NULL,
    subrows   TEXT NOT NULL
);
";

/// RFC-3339 timestamp, matching the format the run event log uses.
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// A 12-character lowercase id. Same generator idea as pilot's run ids
/// (`pilot::new_run_id`), minus the date prefix — a card id is typed by hand
/// as a prefix, so every character should carry entropy.
pub fn new_id() -> String {
    use rand::distr::SampleString;
    rand::distr::Alphanumeric
        .sample_string(&mut rand::rng(), 12)
        .to_ascii_lowercase()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tempfile::tempdir;

    pub(crate) fn store() -> (tempfile::TempDir, BoardStore) {
        let dir = tempdir().unwrap();
        let store = BoardStore::open(&dir.path().join("board.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn open_creates_schema_and_version() {
        let (_d, s) = store();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
        let conn = s.lock();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // meta, project, card, dispatch, rollup, sqlite_sequence
        assert!(n >= 5, "expected the board tables, found {n}");
    }

    #[test]
    fn reopen_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("board.db");
        BoardStore::open(&path).unwrap();
        let s = BoardStore::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn meta_round_trips() {
        let (_d, s) = store();
        assert_eq!(s.get_meta("nope").unwrap(), None);
        s.set_meta("k", "v1").unwrap();
        s.set_meta("k", "v2").unwrap();
        assert_eq!(s.get_meta("k").unwrap().as_deref(), Some("v2"));
    }

    #[test]
    fn ids_are_unique_and_lowercase() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 12);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }
}
