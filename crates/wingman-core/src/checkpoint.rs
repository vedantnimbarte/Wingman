//! File-edit checkpoints for `/undo`.
//!
//! Before a mutating tool runs, the dispatcher [`capture`]s the target
//! file's prior bytes; on success it [`commit`]s them as a new checkpoint.
//! `/undo` calls [`undo_last`] to restore the most recent one. Snapshots and
//! a JSONL manifest live under `<project>/.wingman/checkpoints/`.
//!
//! Each mutating tool call is one undo step, newest first — `/undo` twice
//! walks back two edits.
//!
//! The same entries are the rewind timeline. A surface that owns a session
//! calls [`set_turn`] before each turn, so every checkpoint names the session
//! and turn that made it; [`timeline`] groups them into one point per turn,
//! [`preview`] shows what restoring to a point would change, and
//! [`restore_to`] does it. Restoring never deletes a checkpoint — it writes
//! one of its own, so a restore is undone like any other edit.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
    /// The session and turn that made the edit, from [`set_turn`]. Absent on
    /// entries from a surface that does not tag them, and on older entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn: Option<usize>,
    /// Set on the entries [`restore_to`] writes: the seq it restored to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restore: Option<u64>,
}

/// The session and turn the next checkpoints belong to.
///
/// Process-wide because the dispatcher that commits checkpoints knows neither.
/// Every surface that sets it (the TUI, `--print`) runs one session per
/// process, so there is nothing for two sessions to race over.
static TURN: Mutex<Option<(String, usize)>> = Mutex::new(None);

/// Tag the checkpoints committed from now on with `session` and `turn`
/// (0-based, counted the way `wingman_session::turn_starts` counts them).
pub fn set_turn(session: &str, turn: usize) {
    *TURN.lock().unwrap_or_else(|e| e.into_inner()) = Some((session.to_string(), turn));
}

fn current_turn() -> (Option<String>, Option<usize>) {
    match TURN.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        Some((session, turn)) => (Some(session), Some(turn)),
        None => (None, None),
    }
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
    let (session, turn) = current_turn();
    let _ = append(root, pres, session, turn, None);
}

/// Write `pres` as checkpoint entries. A snapshot that cannot be written drops
/// only its own entry; the error is still returned, for the one caller
/// ([`restore_to`]) that must not go ahead without them.
fn append(
    root: &Path,
    pres: Vec<Pre>,
    session: Option<String>,
    turn: Option<usize>,
    restore: Option<u64>,
) -> std::io::Result<()> {
    if pres.is_empty() {
        return Ok(());
    }
    let d = dir(root);
    std::fs::create_dir_all(&d)?;
    let mut seq = next_seq(root);
    let ts = Some(now_secs());
    let mut out = String::new();
    let mut failed = None;
    for pre in pres {
        let (snap, existed) = match &pre.prior {
            Some(bytes) => {
                let name = format!("{seq}.snap");
                if let Err(e) = std::fs::write(d.join(&name), bytes) {
                    failed = Some(e);
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
            session: session.clone(),
            turn,
            restore,
        };
        if let Ok(line) = serde_json::to_string(&entry) {
            out.push_str(&line);
            out.push('\n');
            seq += 1;
        }
    }
    // A restore that could not save every file's current state records none
    // of it: a restore point that restored nothing would be a false entry.
    if let (Some(e), Some(_)) = (failed.take(), restore) {
        return Err(e);
    }
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(manifest(root))?
        .write_all(out.as_bytes())?;
    failed.map_or(Ok(()), Err)
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

/// One point on the rewind timeline: the edits one turn made, or one restore.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    /// The point's earliest checkpoint. Restoring to it puts every file back
    /// as it was before this point's first edit.
    pub seq: u64,
    pub session: Option<String>,
    pub turn: Option<usize>,
    /// On a restore's own point: the seq it restored to.
    pub restore: Option<u64>,
    /// Unix seconds of the point's latest edit, if known.
    pub ts: Option<u64>,
    /// Files touched, relative to the project, in first-touched order.
    pub files: Vec<String>,
}

/// The checkpoints grouped into points, newest first.
///
/// A tagged turn is one point even when another process's edits landed
/// between its own. Restores and untagged edits only join the point directly
/// before them, since nothing else says which edits belong together.
pub fn timeline(root: &Path) -> Vec<Point> {
    let mut points: Vec<Point> = Vec::new();
    for e in read_entries(root) {
        let same = |p: &Point| p.session == e.session && p.turn == e.turn && p.restore == e.restore;
        let at = if e.session.is_some() && e.turn.is_some() && e.restore.is_none() {
            points.iter().rposition(same)
        } else {
            points.len().checked_sub(1).filter(|&i| same(&points[i]))
        };
        let file = relative(root, Path::new(&e.path));
        match at {
            Some(i) => {
                let point = &mut points[i];
                point.ts = e.ts.or(point.ts);
                if !point.files.contains(&file) {
                    point.files.push(file);
                }
            }
            None => points.push(Point {
                seq: e.seq,
                session: e.session,
                turn: e.turn,
                restore: e.restore,
                ts: e.ts,
                files: vec![file],
            }),
        }
    }
    points.reverse();
    points
}

/// What restoring to a point would change in one file.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// Relative to the project.
    pub path: String,
    pub exists_now: bool,
    pub exists_after: bool,
    /// Unified diff from the file as it is to the file as it will be, without
    /// file headers. `(binary file)` when either side is not UTF-8.
    pub diff: String,
}

/// One file a restore writes: its bytes now, and the bytes it will have
/// (`None` = absent).
struct Planned {
    path: PathBuf,
    now: Option<Vec<u8>>,
    then: Option<Vec<u8>>,
}

/// Work out a restore to `seq` without writing anything.
///
/// Each file touched at or after `seq` goes back to the pre-image of its
/// earliest entry there. Files already in that state are left out. The
/// manifest is a file in the repo, so its paths and snapshot names are checked
/// rather than trusted: a path outside the project, or a snapshot name that is
/// not a bare file name, fails the whole plan.
fn plan(root: &Path, seq: u64) -> Result<Vec<Planned>, String> {
    let entries: Vec<Entry> = read_entries(root)
        .into_iter()
        .filter(|e| e.seq >= seq)
        .collect();
    if entries.is_empty() {
        return Err(format!("no checkpoint #{seq}"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    // Manifest order is seq order, so the first entry per path is its earliest.
    for e in entries {
        if !seen.insert(e.path.clone()) {
            continue;
        }
        let path = PathBuf::from(&e.path);
        if inside(root, &path).is_none() {
            return Err(format!(
                "refusing to restore {}: it is outside the project",
                e.path
            ));
        }
        let then = if e.existed {
            let snap = e
                .snap
                .as_deref()
                .filter(|s| Path::new(s).file_name() == Some(std::ffi::OsStr::new(s)))
                .ok_or_else(|| format!("checkpoint #{} has no usable snapshot", e.seq))?;
            let bytes = std::fs::read(dir(root).join(snap))
                .map_err(|err| format!("snapshot for {} is unreadable: {err}", e.path))?;
            Some(bytes)
        } else {
            None
        };
        let now = std::fs::read(&path).ok();
        if now != then {
            out.push(Planned { path, now, then });
        }
    }
    Ok(out)
}

/// What [`restore_to`] would change, file by file. Writes nothing.
pub fn preview(root: &Path, seq: u64) -> Result<Vec<Change>, String> {
    Ok(plan(root, seq)?
        .into_iter()
        .map(|p| {
            let diff = match (
                std::str::from_utf8(p.now.as_deref().unwrap_or_default()),
                std::str::from_utf8(p.then.as_deref().unwrap_or_default()),
            ) {
                (Ok(now), Ok(then)) => similar::TextDiff::from_lines(now, then)
                    .unified_diff()
                    .context_radius(3)
                    .to_string(),
                _ => "(binary file)".to_string(),
            };
            Change {
                path: relative(root, &p.path),
                exists_now: p.now.is_some(),
                exists_after: p.then.is_some(),
                diff,
            }
        })
        .collect())
}

/// Put every file back as it was before checkpoint `seq`, undoing that
/// checkpoint and everything after it. Returns one line per file written;
/// empty when the files are already in that state.
///
/// Never deletes a checkpoint. The files' current state is committed first,
/// as a checkpoint tagged `session` that records the restore, so the restore
/// is on the timeline and undoable like any edit. If that checkpoint cannot be
/// written, or the plan fails, nothing is restored.
pub fn restore_to(root: &Path, seq: u64, session: Option<&str>) -> Result<Vec<String>, String> {
    let planned = plan(root, seq)?;
    let pres = planned
        .iter()
        .map(|p| Pre {
            path: p.path.clone(),
            prior: p.now.clone(),
        })
        .collect();
    append(root, pres, session.map(str::to_string), None, Some(seq)).map_err(|e| {
        format!("could not checkpoint the current state, so nothing was restored: {e}")
    })?;
    let mut lines = Vec::new();
    for p in planned {
        let name = relative(root, &p.path);
        let written = match &p.then {
            Some(bytes) => p
                .path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|_| std::fs::write(&p.path, bytes)),
            None => std::fs::remove_file(&p.path),
        };
        written.map_err(|e| format!("restoring {name}: {e}"))?;
        lines.push(match (&p.now, &p.then) {
            (_, None) => format!("removed {name}"),
            (None, Some(_)) => format!("recreated {name}"),
            (Some(_), Some(_)) => format!("restored {name}"),
        });
    }
    Ok(lines)
}

/// `path` relative to the project, or `None` when it is not inside it.
///
/// Compared canonically. Surfaces spell the same directory differently —
/// `wingman serve` canonicalises its roots (`\\?\C:\repo`, `/private/tmp/repo`)
/// while the TUI records paths under the directory it started in (`C:\repo`,
/// `/tmp/repo`) — and a symlink in the tree that points out of it has to count
/// as outside. A path that does not exist yet (a file a restore recreates) is
/// resolved through its nearest existing ancestor.
fn inside(root: &Path, path: &Path) -> Option<PathBuf> {
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let mut probe = path;
    let mut missing = Vec::new();
    let resolved = loop {
        if let Ok(c) = probe.canonicalize() {
            break c;
        }
        missing.push(probe.file_name()?);
        probe = probe.parent()?;
    };
    let mut rel = resolved.strip_prefix(&root).ok()?.to_path_buf();
    rel.extend(missing.into_iter().rev());
    Some(rel)
}

/// `path` relative to the project, with `/` separators, for display.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .map(Path::to_path_buf)
        .or_else(|| inside(root, path))
        .as_deref()
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
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

    /// One edit, committed under an explicit tag (the process-wide one is
    /// left alone so parallel tests cannot see each other's).
    fn edit(root: &Path, file: &str, content: Option<&str>, turn: usize) {
        let pre = capture(root, file);
        match content {
            Some(c) => std::fs::write(root.join(file), c).unwrap(),
            None => std::fs::remove_file(root.join(file)).unwrap(),
        }
        append(root, vec![pre], Some("s1".into()), Some(turn), None).unwrap();
    }

    fn read(root: &Path, file: &str) -> Option<String> {
        std::fs::read_to_string(root.join(file)).ok()
    }

    #[test]
    fn restore_to_a_turn_is_itself_a_checkpoint_and_can_be_undone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "v0\n").unwrap();

        edit(root, "a.txt", Some("v1\n"), 0);
        edit(root, "b.txt", Some("new\n"), 0);
        edit(root, "a.txt", Some("v2\n"), 1);

        let points = timeline(root);
        assert_eq!(points.len(), 2, "one point per turn");
        assert_eq!(points[0].turn, Some(1));
        assert_eq!(points[0].files, vec!["a.txt"]);
        assert_eq!(points[1].files, vec!["a.txt", "b.txt"]);
        let turn0 = points[1].seq;

        // The preview is what restoring would do, and writes nothing.
        let changes = preview(root, turn0).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(changes[0].diff.contains("-v2") && changes[0].diff.contains("+v0"));
        assert!(changes[1].exists_now && !changes[1].exists_after);
        assert_eq!(read(root, "a.txt").as_deref(), Some("v2\n"));

        let lines = restore_to(root, turn0, Some("s1")).unwrap();
        assert_eq!(lines, vec!["restored a.txt", "removed b.txt"]);
        assert_eq!(read(root, "a.txt").as_deref(), Some("v0\n"));
        assert_eq!(read(root, "b.txt"), None);

        // Nothing was deleted: the restore added its own point on top.
        assert_eq!(depth(root), 5);
        let points = timeline(root);
        assert_eq!(points.len(), 3);
        assert_eq!(points[0].restore, Some(turn0));
        assert_eq!(points[0].session.as_deref(), Some("s1"));

        // Restoring to before the restore undoes it.
        let undo = points[0].seq;
        restore_to(root, undo, Some("s1")).unwrap();
        assert_eq!(read(root, "a.txt").as_deref(), Some("v2\n"));
        assert_eq!(read(root, "b.txt").as_deref(), Some("new\n"));

        // Already there: nothing to write, and no empty checkpoint for it.
        let depth_before = depth(root);
        assert!(restore_to(root, undo, Some("s1")).unwrap().is_empty());
        assert_eq!(depth(root), depth_before);
        assert!(preview(root, 999).is_err());
    }

    #[test]
    fn a_turn_split_by_another_process_is_still_one_point() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        edit(root, "a.txt", Some("1"), 0);
        let pre = capture(root, "c.txt");
        std::fs::write(root.join("c.txt"), "x").unwrap();
        append(root, vec![pre], None, None, None).unwrap();
        edit(root, "b.txt", Some("2"), 0);

        let points = timeline(root);
        assert_eq!(points.len(), 2);
        assert_eq!(points[1].files, vec!["a.txt", "b.txt"]);
        assert_eq!(points[0].session, None);
    }

    #[test]
    fn set_turn_tags_what_commit_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        set_turn("tagged", 4);
        let pre = capture(root, "a.txt");
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, vec![pre]);
        let point = &timeline(root)[0];
        assert_eq!(point.session.as_deref(), Some("tagged"));
        assert_eq!(point.turn, Some(4));
    }

    #[test]
    fn a_manifest_pointing_outside_the_project_restores_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "now").unwrap();
        edit(&root, "a.txt", Some("later"), 0);
        // A hand-written entry for a file outside the project, after the edit.
        let outside = tmp.path().join("victim.txt");
        std::fs::write(&outside, "keep").unwrap();
        let line = format!(
            "{{\"seq\":1,\"path\":{},\"snap\":null,\"existed\":false}}\n",
            serde_json::to_string(&outside.to_string_lossy()).unwrap()
        );
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(manifest(&root))
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();

        let err = restore_to(&root, 0, None).unwrap_err();
        assert!(err.contains("outside the project"), "{err}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
        assert_eq!(read(&root, "a.txt").as_deref(), Some("later"));
        assert_eq!(depth(&root), 2, "no checkpoint written for a refused plan");
    }

    /// The TUI records paths under the directory it started in; `wingman
    /// serve` restores against its canonicalised root (`\\?\C:\…` on Windows,
    /// `/private/var/…` on macOS). Both name the same project.
    #[test]
    fn a_root_spelled_canonically_still_restores_what_the_tui_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "v0\n").unwrap();
        edit(root, "a.txt", Some("v1\n"), 0);
        edit(root, "new.txt", Some("x\n"), 0);

        let canonical = root.canonicalize().unwrap();
        let points = timeline(&canonical);
        assert_eq!(points[0].files, vec!["a.txt", "new.txt"]);
        let lines = restore_to(&canonical, points[0].seq, None).unwrap();
        assert_eq!(lines, vec!["restored a.txt", "removed new.txt"]);
        assert_eq!(read(root, "a.txt").as_deref(), Some("v0\n"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_project_restores_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let outside = tmp.path().join("victim.txt");
        std::fs::write(&outside, "keep").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link.txt")).unwrap();
        // A checkpoint of "link.txt", so a restore would write its snapshot
        // through the link to the file it points at.
        let pre = Pre {
            path: root.join("link.txt"),
            prior: Some(b"planted".to_vec()),
        };
        append(&root, vec![pre], None, None, None).unwrap();

        let err = restore_to(&root, 0, None).unwrap_err();
        assert!(err.contains("outside the project"), "{err}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
    }
}
