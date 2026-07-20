//! Speculative pre-read: when the agent reads a file, warm the OS page cache
//! for the source files it is *likely* to touch next, so the following
//! `read_file`/`grep` hits warm cache instead of cold disk. Fire-and-forget on
//! a blocking thread — never on the request path — so it's near-free.
//!
//! ponytail: same-directory heuristic (a module's siblings are the usual next
//! reads). Upgrade path is index proximity — prefetch the top semantic
//! neighbours of the just-read file. Also pre-warms `git status` once.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Cap how many siblings we warm per read, and skip large files (warming a
/// generated 5 MB file wastes IO for a read that probably won't happen).
const MAX_SIBLINGS: usize = 12;
const MAX_FILE_BYTES: u64 = 512 * 1024;

fn warmed() -> &'static Mutex<HashSet<PathBuf>> {
    static S: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Source-like sibling files in the same directory as `path` (excluding
/// `path`), sorted and capped at `max`. Pure — the unit of behaviour we test.
pub fn sibling_candidates(path: &Path, max: usize) -> Vec<PathBuf> {
    let Some(dir) = path.parent() else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| p != path && is_source_like(p))
        .collect();
    out.sort();
    out.truncate(max);
    out
}

fn is_source_like(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()),
        Some(
            "rs" | "py"
                | "js"
                | "ts"
                | "tsx"
                | "jsx"
                | "go"
                | "java"
                | "c"
                | "h"
                | "cpp"
                | "hpp"
                | "rb"
                | "toml"
                | "json"
                | "yaml"
                | "yml"
                | "md"
                | "css"
                | "html"
        )
    )
}

/// Warm the page cache for `path`'s siblings on a background thread. Each file
/// is warmed at most once per process. Requires a tokio runtime (called from
/// the async tool path); a no-op if none is present.
pub fn warm_siblings(path: PathBuf) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::task::spawn_blocking(move || {
        let cands = sibling_candidates(&path, MAX_SIBLINGS);
        // Reserve the fresh ones under the lock, read outside it.
        let to_read: Vec<PathBuf> = {
            let mut w = warmed().lock().unwrap_or_else(|e| e.into_inner());
            cands.into_iter().filter(|c| w.insert(c.clone())).collect()
        };
        for c in to_read {
            if std::fs::metadata(&c)
                .map(|m| m.len() <= MAX_FILE_BYTES)
                .unwrap_or(false)
            {
                let _ = std::fs::read(&c); // pull into page cache, discard
            }
        }
    });
}

/// Pre-warm `git status` once per process so the first status-dependent
/// operation (diff, checkpoint) doesn't pay the cold cost. No-op off-runtime.
pub fn warm_git_status_once(root: PathBuf) {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::task::spawn_blocking(move || {
        let _ = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&root)
            .output();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_candidates_picks_source_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("a.rs"), "").unwrap();
        std::fs::write(d.join("b.rs"), "").unwrap();
        std::fs::write(d.join("notes.md"), "").unwrap();
        std::fs::write(d.join("image.png"), "").unwrap();
        std::fs::create_dir(d.join("sub")).unwrap();

        let cands = sibling_candidates(&d.join("a.rs"), 12);
        let names: Vec<String> = cands
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"b.rs".to_string()));
        assert!(names.contains(&"notes.md".to_string()));
        assert!(!names.contains(&"a.rs".to_string())); // excludes self
        assert!(!names.contains(&"image.png".to_string())); // not source-like
        assert!(!names.contains(&"sub".to_string())); // dirs excluded
    }

    #[test]
    fn respects_max() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            std::fs::write(dir.path().join(format!("f{i}.rs")), "").unwrap();
        }
        assert_eq!(sibling_candidates(&dir.path().join("f0.rs"), 5).len(), 5);
    }
}
