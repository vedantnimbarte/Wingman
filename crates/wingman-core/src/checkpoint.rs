//! File-edit checkpoints for `/undo`.
//!
//! Before a mutating tool runs, the dispatcher [`capture`]s the target
//! file's prior bytes; on success it [`commit`]s them as a new checkpoint.
//! `/undo` calls [`undo_last`] to restore the most recent one. Snapshots and
//! a JSONL manifest live under `<project>/.wingman/checkpoints/`.
//!
//! Each mutating tool call is one undo step, newest first — `/undo` twice
//! walks back two edits.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

fn dir(root: &Path) -> PathBuf {
    root.join(".wingman").join("checkpoints")
}

fn manifest(root: &Path) -> PathBuf {
    dir(root).join("log.jsonl")
}

/// A captured pre-edit state of one file, held in memory until the edit is
/// known to have succeeded.
pub struct Pre {
    path: PathBuf,
    /// `None` means the file did not exist before the edit (it was created).
    prior: Option<Vec<u8>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    seq: u64,
    path: String,
    /// Snapshot filename holding prior bytes, or `None` if the file was new.
    snap: Option<String>,
    existed: bool,
    /// Unix seconds when the checkpoint was committed. `None` on entries
    /// written before timestamps were added.
    #[serde(default)]
    ts: Option<u64>,
}

/// One entry in the rewind timeline, newest-first when returned by [`list`].
#[derive(Debug, Clone)]
pub struct Step {
    pub seq: u64,
    pub path: String,
    /// True if the file was modified; false if the edit created it (undo
    /// deletes it).
    pub existed: bool,
    /// Unix seconds when committed, if known.
    pub ts: Option<u64>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Which file path(s) a tool call will mutate. Empty for non-mutating tools.
pub fn mutating_paths(name: &str, args: &serde_json::Value) -> Vec<String> {
    match name {
        "write_file" | "edit_file" | "edit_symbol" => args
            .get("path")
            .and_then(|p| p.as_str())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
        "apply_patch" => args
            .get("patch")
            .and_then(|p| p.as_str())
            .map(patch_paths)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn patch_paths(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|l| {
            for pfx in ["*** Update File: ", "*** Add File: ", "*** Delete File: "] {
                if let Some(rest) = l.trim().strip_prefix(pfx) {
                    return Some(rest.trim().to_string());
                }
            }
            None
        })
        .collect()
}

fn resolve(root: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

/// Read the current bytes of `path` into memory (or record that it's absent).
pub fn capture(root: &Path, path: &str) -> Pre {
    let abs = resolve(root, path);
    let prior = std::fs::read(&abs).ok();
    Pre { path: abs, prior }
}

/// Persist captured pre-images as one new checkpoint. Call only after the
/// edit succeeded. Best-effort: any IO error is swallowed (undo just won't
/// have that entry) so checkpointing never breaks the edit path.
pub fn commit(root: &Path, pres: Vec<Pre>) {
    if pres.is_empty() {
        return;
    }
    let d = dir(root);
    if std::fs::create_dir_all(&d).is_err() {
        return;
    }
    let mut seq = next_seq(root);
    let ts = Some(now_secs());
    let mut out = String::new();
    for pre in pres {
        let (snap, existed) = match &pre.prior {
            Some(bytes) => {
                let name = format!("{seq}.snap");
                if std::fs::write(d.join(&name), bytes).is_err() {
                    continue;
                }
                (Some(name), true)
            }
            None => (None, false),
        };
        let entry = Entry {
            seq,
            path: pre.path.to_string_lossy().into_owned(),
            snap,
            existed,
            ts,
        };
        if let Ok(line) = serde_json::to_string(&entry) {
            out.push_str(&line);
            out.push('\n');
            seq += 1;
        }
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(manifest(root))
    {
        let _ = f.write_all(out.as_bytes());
    }
}

fn read_entries(root: &Path) -> Vec<Entry> {
    std::fs::read_to_string(manifest(root))
        .ok()
        .map(|s| {
            s.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn next_seq(root: &Path) -> u64 {
    read_entries(root).last().map(|e| e.seq + 1).unwrap_or(0)
}

/// Number of undo steps currently available.
pub fn depth(root: &Path) -> usize {
    read_entries(root).len()
}

/// Restore the most recent checkpoint: rewrite the file with its prior bytes,
/// or delete it if the edit had created it. Returns a short human summary, or
/// `None` if there's nothing to undo.
pub fn undo_last(root: &Path) -> Option<String> {
    let mut entries = read_entries(root);
    let last = entries.pop()?;
    let path = PathBuf::from(&last.path);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| last.path.clone());

    let summary = if last.existed {
        let snap = last.snap.as_ref()?;
        let bytes = std::fs::read(dir(root).join(snap)).ok()?;
        std::fs::write(&path, bytes).ok()?;
        let _ = std::fs::remove_file(dir(root).join(snap));
        format!("reverted {name}")
    } else {
        let _ = std::fs::remove_file(&path);
        format!("removed {name} (was newly created)")
    };

    // Rewrite the manifest without the entry we just undid.
    let rest: String = entries
        .iter()
        .filter_map(|e| serde_json::to_string(e).ok())
        .map(|l| format!("{l}\n"))
        .collect();
    let _ = std::fs::write(manifest(root), rest);
    Some(summary)
}

/// The rewind timeline, newest edit first. Each entry is one undo step.
pub fn list(root: &Path) -> Vec<Step> {
    let mut steps: Vec<Step> = read_entries(root)
        .into_iter()
        .map(|e| Step {
            seq: e.seq,
            path: e.path,
            existed: e.existed,
            ts: e.ts,
        })
        .collect();
    steps.reverse();
    steps
}

/// Rewind the last `n` checkpoints (each a single mutating edit), newest
/// first. Returns one summary line per reverted step. Stops early if the
/// timeline runs out.
pub fn undo_n(root: &Path, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    for _ in 0..n {
        match undo_last(root) {
            Some(s) => out.push(s),
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_commit_undo_roundtrip() {
        let root = std::env::temp_dir().join(format!("wingman-ckpt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // Existing file: edit then undo restores original content.
        std::fs::write(root.join("a.txt"), "original").unwrap();
        let pre = capture(&root, "a.txt");
        std::fs::write(root.join("a.txt"), "edited").unwrap();
        commit(&root, vec![pre]);
        assert_eq!(depth(&root), 1);
        assert_eq!(undo_last(&root).as_deref(), Some("reverted a.txt"));
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "original"
        );
        assert_eq!(depth(&root), 0);

        // New file: capture (absent) then create then undo removes it.
        let pre = capture(&root, "b.txt");
        std::fs::write(root.join("b.txt"), "new").unwrap();
        commit(&root, vec![pre]);
        assert!(undo_last(&root).unwrap().contains("removed b.txt"));
        assert!(!root.join("b.txt").exists());

        // Nothing left to undo.
        assert!(undo_last(&root).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_and_undo_n_walk_the_timeline() {
        let root = std::env::temp_dir().join(format!("wingman-tl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        for (i, content) in ["v1", "v2", "v3"].iter().enumerate() {
            let pre = capture(&root, "a.txt");
            std::fs::write(root.join("a.txt"), content).unwrap();
            commit(&root, vec![pre]);
            let _ = i;
        }
        // Timeline is newest-first and carries timestamps.
        let steps = list(&root);
        assert_eq!(steps.len(), 3);
        assert!(steps[0].seq > steps[2].seq);
        assert!(steps[0].ts.is_some());

        // Rewind two steps: v3→v2 undone leaves the file as it was before v2's
        // edit, i.e. "v1".
        let summaries = undo_n(&root, 2);
        assert_eq!(summaries.len(), 2);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "v1");
        assert_eq!(depth(&root), 1);

        // Asking for more than remain stops cleanly.
        assert_eq!(undo_n(&root, 5).len(), 1);
        assert_eq!(depth(&root), 0);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn extracts_apply_patch_paths() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Add File: src/b.rs\n+hi\n*** End Patch";
        assert_eq!(patch_paths(patch), vec!["src/a.rs", "src/b.rs"]);
    }
}
