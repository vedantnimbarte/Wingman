//! Oversized tool output, kept instead of thrown away.
//!
//! [`ToolOutputBudget`](crate::tokens::ToolOutputBudget) caps what a tool
//! result costs the model by keeping the head and tail and eliding the middle.
//! That bound is necessary — a 4000-line test run would otherwise crowd out
//! the conversation — but on its own it is lossy in a way the model cannot
//! recover from: the elided middle is simply gone. The marker used to say the
//! full text was "in the session log", which was an instruction the model had
//! no tool to act on.
//!
//! Spilling closes that gap without widening the budget. The complete output
//! is written to a session-scoped file and the model is handed the path, so it
//! can go back for the part it needs with `read_file`'s `offset`/`limit`.
//! Context cost is unchanged; what changes is that the truncation stops being
//! a one-way door.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Session-scoped storage for tool output too large to send in full.
#[derive(Debug)]
pub struct SpillStore {
    dir: PathBuf,
    /// Names files in call order, so a directory listing reads as a
    /// transcript rather than a bag of hashes.
    seq: AtomicU32,
}

impl SpillStore {
    /// Store spills under `dir`, typically
    /// `<project>/.wingman/spill/<session-id>/`.
    ///
    /// The directory is created lazily on the first save: most sessions never
    /// spill anything, and an empty directory per session is litter.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            seq: AtomicU32::new(1),
        }
    }

    /// Persist `content` and return the file it was written to.
    ///
    /// Best-effort by design: a failed write returns `None` and the caller
    /// falls back to plain truncation. Failing a tool call because a
    /// convenience file could not be written would turn a recoverable
    /// situation into a broken one.
    pub fn save(&self, tool: &str, content: &str) -> Option<PathBuf> {
        if std::fs::create_dir_all(&self.dir).is_err() {
            return None;
        }
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let path = self.dir.join(format!("{n:03}-{}.txt", sanitize(tool)));
        std::fs::write(&path, content).ok()?;
        Some(path)
    }

    /// Where this store writes. Used by callers that want to mention the
    /// directory without having spilled anything yet.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// How long a session's spilled output stays on disk.
///
/// Spills are only useful while the session that produced them is running —
/// the locator is in that conversation's history and nothing else refers to
/// it. Without a sweep, every truncated tool result would accumulate in the
/// user's project directory forever, which is how `.wingman/` quietly becomes
/// the biggest thing in a repo. A week is long enough that resuming a session
/// from a couple of days ago still works.
pub const SPILL_RETENTION_DAYS: u64 = 7;

/// Delete spill directories under `root` older than [`SPILL_RETENTION_DAYS`].
///
/// Best-effort and non-fatal throughout: this is housekeeping, and a session
/// must never fail to start because an old directory could not be removed.
/// Only immediate children of `root` are considered, and only directories, so
/// a stray file there is left alone rather than guessed at.
pub fn sweep_expired(root: &Path) {
    sweep_older_than(
        root,
        std::time::Duration::from_secs(SPILL_RETENTION_DAYS * 24 * 60 * 60),
    )
}

/// [`sweep_expired`] with the retention window as a parameter, so the sweep
/// logic is testable without backdating file timestamps.
pub fn sweep_older_than(root: &Path, max_age: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return; // no spill root yet — nothing to sweep
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let expired = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age >= max_age);
        if expired {
            let path = entry.path();
            if std::fs::remove_dir_all(&path).is_ok() {
                tracing::debug!(
                    target: "wingman::spill",
                    dir = %path.display(),
                    "removed expired spill directory"
                );
            }
        }
    }
}

/// Reduce a tool name to one safe filename segment.
///
/// Tool names are not all ours — MCP servers contribute
/// `mcp__<server>__<tool>`, and a server chooses its own half. Anything
/// outside the allowed set becomes `_`, so a name carrying a separator or a
/// `..` cannot steer the write out of the spill directory.
fn sanitize(tool: &str) -> String {
    let cleaned: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    if cleaned.is_empty() {
        "tool".into()
    } else {
        cleaned
    }
}

/// The line prepended to a spilled tool result, telling the model where the
/// rest went and how to get it.
///
/// Deliberately the **first** line rather than part of the elision marker in
/// the middle: [`ToolResultPruner`](crate::tokens::ToolResultPruner) later
/// rewrites long results to a head and a tail, and a locator buried in the
/// middle is exactly what that would discard. The head always survives.
pub fn locator_line(path: &Path, total_lines: usize) -> String {
    format!(
        "[wingman] Output was {total_lines} lines; the middle is elided below. \
         Full text: {} — re-read any span with read_file(path, offset, limit).",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "wingman-spill-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn saved_output_round_trips_verbatim() {
        let dir = tmp("rt");
        let store = SpillStore::new(dir.clone());
        let body = (0..5000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let path = store.save("run_shell", &body).expect("save");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_save_gets_its_own_file_in_call_order() {
        let dir = tmp("seq");
        let store = SpillStore::new(dir.clone());
        let a = store.save("grep_tool", "first").unwrap();
        let b = store.save("grep_tool", "second").unwrap();
        assert_ne!(a, b, "a second spill must not overwrite the first");
        assert!(a.file_name().unwrap().to_str().unwrap().starts_with("001"));
        assert!(b.file_name().unwrap().to_str().unwrap().starts_with("002"));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "first");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hostile_tool_name_cannot_escape_the_spill_directory() {
        let dir = tmp("esc");
        let store = SpillStore::new(dir.clone());
        let path = store
            .save("../../../etc/passwd", "pwned")
            .expect("save should succeed, just not where the name asked");
        assert_eq!(path.parent().unwrap(), dir.as_path());
        assert!(!std::fs::exists(dir.join("../../../etc/passwd")).unwrap_or(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_parent_is_created_rather_than_failing_the_call() {
        let dir = tmp("deep").join("nested").join("deeper");
        let store = SpillStore::new(dir.clone());
        assert!(store.save("read_file", "x").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod sweep_tests {
    use super::*;
    use std::time::Duration;

    fn seeded_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "wingman-spill-sweep-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for session in ["session-a", "session-b"] {
            let dir = root.join(session);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("001-run_shell.txt"), "output").unwrap();
        }
        root
    }

    #[test]
    fn sessions_past_the_window_are_removed() {
        let root = seeded_root("old");
        // Everything on disk is at least zero seconds old.
        sweep_older_than(&root, Duration::ZERO);
        assert!(root.exists(), "the root itself must survive");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sessions_inside_the_window_are_kept() {
        let root = seeded_root("new");
        sweep_older_than(&root, Duration::from_secs(3600));
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            2,
            "a fresh session must not be swept out from under a running agent"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stray_file_beside_the_session_dirs_is_left_alone() {
        let root = seeded_root("stray");
        let note = root.join("README");
        std::fs::write(&note, "not a session").unwrap();
        sweep_older_than(&root, Duration::ZERO);
        assert!(note.exists(), "only directories are swept");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweeping_a_root_that_does_not_exist_is_not_an_error() {
        sweep_expired(&std::env::temp_dir().join("wingman-spill-does-not-exist-xyz"));
    }
}
