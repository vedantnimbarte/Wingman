//! Opening a Wingman SQLite database.
//!
//! Every store here is reached by more than one connection at a time, and
//! SQLite's defaults are wrong for that:
//!
//! - `learn.db` is opened separately by the agent's learning hook (which holds
//!   it for the whole session), by `/feedback` and `/skill stats` in the TUI,
//!   by `--print`'s routing stats, and by `wingman router`. In one interactive
//!   session `/feedback` writes through a *second* connection while the hook
//!   still holds the first.
//! - `sessions.db` and `index.db` are written by background indexing tasks —
//!   `drain_pending`, the project backfill — while the foreground is querying
//!   them.
//!
//! Under the default rollback journal a writer excludes readers, and with no
//! busy timeout the loser gets `SQLITE_BUSY` **immediately** rather than
//! waiting. Several call sites discard the error (`let _ = …`), so the symptom
//! is not a crash but a skill outcome or a feedback rating that silently does
//! not persist.
//!
//! WAL lets readers and a writer proceed together; the busy timeout makes a
//! genuine conflict wait instead of failing instantly. `wingman-board` already
//! did both and its own comment noted that `learn.db` did not — this is that
//! gap closed, in one place rather than three.

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

/// How long a blocked statement waits before giving up.
///
/// Long enough to cover another connection's write — these are small
/// transactions — and short enough that a genuinely wedged database surfaces
/// as an error rather than an unexplained hang.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Open a SQLite database configured for Wingman's concurrent access.
///
/// Creates the parent directory if missing, then enables WAL and a busy
/// timeout. Callers create their own schema afterwards; this only handles the
/// part every store needs and each was otherwise free to forget.
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        // Not fatal on its own — `Connection::open` will produce the better
        // error if the directory truly cannot be used.
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    apply_pragmas(&conn)?;
    Ok(conn)
}

/// Apply the concurrency pragmas to an already-open connection.
///
/// Separate so a store that opens its connection some other way (in memory,
/// read-only, through a different constructor) can still get them.
pub fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    // `journal_mode` returns a row, so it cannot go through `execute_batch`
    // without tripping "Execute returned results".
    //
    // WAL is a property of the database file, not the connection: setting it
    // once sticks. Re-asserting it on every open is harmless and means a
    // database created before this existed gets upgraded the next time it is
    // opened, rather than staying on the old journal forever.
    let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "wingman-sqlite-{tag}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn an_opened_database_is_in_wal_mode() {
        let path = tmp("wal");
        let conn = open(&path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    /// The case that motivated this: two connections to one database, one
    /// writing while the other is open. Under the default journal with no busy
    /// timeout the second writer fails immediately.
    #[test]
    fn a_second_connection_can_write_while_the_first_is_open() {
        let path = tmp("concurrent");
        let first = open(&path).unwrap();
        first
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);")
            .unwrap();
        first
            .execute("INSERT INTO t(v) VALUES ('from-first')", [])
            .unwrap();

        // Second connection, first still open — this is `/feedback` writing
        // while the agent's learning hook holds its own handle.
        let second = open(&path).unwrap();
        second
            .execute("INSERT INTO t(v) VALUES ('from-second')", [])
            .expect("a second writer must not be refused outright");

        let count: i64 = first
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2, "both writes should have landed");

        drop(first);
        drop(second);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn opening_creates_a_missing_parent_directory() {
        let dir = tmp("nested").with_extension("");
        let path = dir.join("deeper").join("store.db");
        let conn = open(&path).unwrap();
        drop(conn);
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
