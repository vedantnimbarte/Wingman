//! Mutation spot-check gate stage (`[verify].mutation`): flip one simple
//! operator on a changed line, run that file's tests, put the file back. A
//! mutant the tests still pass on is a change the tests don't pin down.
//!
//! This rewrites the user's source files, so the restore path is the part that
//! matters: the original goes to a durable backup under `.wingman/` before the
//! file is touched, a drop guard restores it on every exit (error, timeout,
//! panic, the gate future being dropped), and a backup left by a killed
//! process is restored at the next gate or session start.

use std::collections::{BTreeSet, HashMap};
use std::io::{ErrorKind, Write};
use std::path::{Component, Path};
use std::time::{Duration, Instant};

use wingman_config::MutationVerifyConfig;
use wingman_core::{GateReport, TurnGate};

use crate::coverage::turn_changed_lines;
use crate::runtime::{crate_name_for, run_check_cmd};

/// The mutant in flight: `<repo-relative path>\n<original length>\n` then the
/// original bytes, then the mutated bytes. ponytail: one fixed slot, so one
/// mutating gate per checkout at a time (pilot workers each have their own
/// worktree); a per-process slot if that ever stops holding.
const BACKUP: &str = ".wingman/mutation-backup";

/// Operator swaps. The binary operators carry their surrounding spaces, which
/// keeps `->`, `+=`, `===`, unary minus and most generics (`Vec<u8>`) out.
/// ponytail: textual, so a spaced trait bound (`A + B`) is still a candidate;
/// that mutant fails to compile and counts as killed.
const SWAPS: &[(&str, &str)] = &[
    (" == ", " != "),
    (" != ", " == "),
    (" < ", " >= "),
    (" >= ", " < "),
    (" > ", " <= "),
    (" <= ", " > "),
    (" && ", " || "),
    (" || ", " && "),
    (" + ", " - "),
    (" - ", " + "),
    ("true", "false"),
    ("false", "true"),
];

#[derive(Debug, Clone, PartialEq)]
struct Mutant {
    line: u32,
    /// Byte offset of `from` in the file.
    at: usize,
    from: &'static str,
    to: &'static str,
}

pub struct MutationGate {
    pub root: std::path::PathBuf,
    pub cfg: MutationVerifyConfig,
}

#[async_trait::async_trait]
impl TurnGate for MutationGate {
    fn label(&self) -> String {
        "mutation spot-check".into()
    }

    async fn check(&self) -> GateReport {
        let root = self.root.as_path();
        let mut notes: Vec<String> = Vec::new();
        // Never mutate on top of a leftover: a second backup would overwrite
        // the only copy of the first file's original.
        match recover(root) {
            Ok(Some(file)) => notes.push(format!("restored {file} from an interrupted run")),
            Ok(None) => {}
            Err(e) => {
                return GateReport {
                    passed: false,
                    summary: format!("mutation: ✗ {e}"),
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(self.cfg.timeout_secs);

        // Snapshot every candidate file now; one that differs later was
        // changed by someone else mid-gate and is not ours to touch.
        let mut changed: Vec<_> = turn_changed_lines(root).into_iter().collect();
        changed.sort();
        let mut snapshots: HashMap<String, String> = HashMap::new();
        let mut plan: Vec<(String, Mutant)> = Vec::new();
        for (rel, lines) in changed {
            let Some(lang) = wingman_ts::Language::from_path(Path::new(&rel)) else {
                continue;
            };
            if test_cmd_for(root, &rel).is_none() {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(root.join(&rel)) else {
                continue;
            };
            let spans = wingman_ts::literal_spans(lang, &src);
            plan.extend(
                mutants_in(&src, &lines, &spans)
                    .into_iter()
                    .map(|m| (rel.clone(), m)),
            );
            snapshots.insert(rel, src);
        }
        let candidates = plan.len();
        plan.truncate(self.cfg.max_mutants as usize);
        if plan.is_empty() {
            return GateReport {
                passed: true,
                summary: "mutation: none (no mutable operator on changed Rust/Go lines)".into(),
            };
        }

        let mut baseline: HashMap<String, bool> = HashMap::new();
        let (mut killed, mut timed_out, mut not_run) = (0usize, 0usize, 0usize);
        let mut survived: Vec<String> = Vec::new();
        for (rel, m) in &plan {
            let (Some(cmd), Some(snapshot)) = (test_cmd_for(root, rel), snapshots.get(rel)) else {
                continue;
            };
            // Tests red before any mutant would "kill" every mutant.
            if !baseline.contains_key(&cmd) {
                let left = deadline.saturating_duration_since(Instant::now());
                let Ok(r) = tokio::time::timeout(left, run_check_cmd(&cmd, root)).await else {
                    not_run += 1;
                    continue;
                };
                if !r.passed {
                    notes.push(format!(
                        "`{cmd}` fails without a mutant; skipped its mutants"
                    ));
                }
                baseline.insert(cmd.clone(), r.passed);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if !baseline[&cmd] || left.is_zero() {
                not_run += 1;
                continue;
            }
            if std::fs::read(root.join(rel)).ok().as_deref() != Some(snapshot.as_bytes()) {
                let note = format!("{rel} changed since the gate began; left alone");
                if !notes.contains(&note) {
                    notes.push(note);
                }
                not_run += 1;
                continue;
            }

            let outcome = match apply(root, rel, snapshot, &mutate(snapshot, m)) {
                Ok(_restore_on_drop) => {
                    Ok(tokio::time::timeout(left, run_check_cmd(&cmd, root)).await)
                }
                Err(e) => Err(e),
            };
            // The guard has dropped by here. A backup still on disk means the
            // restore failed: stop, and say where the original is.
            if root.join(BACKUP).exists() {
                return GateReport {
                    passed: false,
                    summary: format!(
                        "mutation: ✗ could not restore {rel} after a mutant; its original is \
                         kept in {BACKUP} — put it back before continuing"
                    ),
                };
            }
            match outcome {
                Ok(Ok(r)) if r.passed => survived.push(format!(
                    "{rel}:{} `{}` → `{}`",
                    m.line,
                    m.from.trim(),
                    m.to.trim()
                )),
                Ok(Ok(_)) => killed += 1,
                Ok(Err(_elapsed)) => timed_out += 1,
                Err(e) => {
                    notes.push(format!("could not apply a mutant to {rel}: {e}"));
                    break;
                }
            }
        }

        let passed = !(self.cfg.fail_on_survivor && !survived.is_empty());
        let mark = if passed { "✓" } else { "✗" };
        let mut s = format!(
            "mutation: {mark} mutants {killed}/{} killed",
            killed + survived.len()
        );
        if timed_out > 0 {
            s.push_str(&format!(", {timed_out} timed out"));
        }
        if not_run > 0 {
            s.push_str(&format!(", {not_run} not run"));
        }
        if candidates > plan.len() {
            s.push_str(&format!(
                " ({} of {candidates} candidates, max_mutants)",
                plan.len()
            ));
        }
        if !survived.is_empty() {
            s.push_str("\nsurvived (no test noticed): ");
            s.push_str(&survived.join(", "));
        }
        for n in notes {
            s.push_str(&format!("\n{n}"));
        }
        GateReport { passed, summary: s }
    }
}

/// Candidate mutants on `lines` (1-based) of `src`, in source order, skipping
/// anything inside `literal` byte spans (comments and strings).
fn mutants_in(src: &str, lines: &BTreeSet<u32>, literal: &[(usize, usize)]) -> Vec<Mutant> {
    let mut out = Vec::new();
    let mut base = 0;
    for (i, text) in src.split_inclusive('\n').enumerate() {
        let line = i as u32 + 1;
        let line_start = base;
        base += text.len();
        if !lines.contains(&line) {
            continue;
        }
        let mut found = Vec::new();
        for &(from, to) in SWAPS {
            for (off, _) in text.match_indices(from) {
                let is_word =
                    |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
                let bytes = text.as_bytes();
                if from.starts_with('t') || from.starts_with('f') {
                    let before = off.checked_sub(1).and_then(|p| bytes.get(p));
                    if is_word(before) || is_word(bytes.get(off + from.len())) {
                        continue;
                    }
                }
                let at = line_start + off;
                if literal.iter().any(|&(s, e)| s <= at && at < e) {
                    continue;
                }
                found.push(Mutant { line, at, from, to });
            }
        }
        found.sort_by_key(|m| m.at);
        out.extend(found);
    }
    out
}

fn mutate(src: &str, m: &Mutant) -> String {
    format!("{}{}{}", &src[..m.at], m.to, &src[m.at + m.from.len()..])
}

/// The applied mutant; dropping it restores the original.
struct RestoreOnDrop<'a>(&'a Path);

impl Drop for RestoreOnDrop<'_> {
    fn drop(&mut self) {
        if let Err(e) = recover(self.0) {
            tracing::error!("mutation gate: {e}");
        }
    }
}

/// Write `mutated` over `rel`, backing `original` up durably first.
fn apply<'a>(
    root: &'a Path,
    rel: &str,
    original: &str,
    mutated: &str,
) -> std::io::Result<RestoreOnDrop<'a>> {
    let backup = root.join(BACKUP);
    if let Some(dir) = backup.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut bytes = format!("{rel}\n{}\n", original.len()).into_bytes();
    bytes.extend_from_slice(original.as_bytes());
    bytes.extend_from_slice(mutated.as_bytes());
    replace_file(&backup, &bytes)?;
    // Armed before the source is touched, so a failed write restores too.
    let guard = RestoreOnDrop(root);
    replace_file(&root.join(rel), mutated.as_bytes())?;
    Ok(guard)
}

/// Write-to-temp, fsync, rename: `path` holds either its old or its new
/// contents, never a torn mix — a half-written backup or source would make
/// the original unrecoverable.
fn replace_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".wingman-tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Restore the file a leftover backup names, then delete the backup. Only
/// when the file still holds exactly the mutant (or already the original):
/// anything else means it was edited meanwhile, and both are left for a
/// human rather than clobbering that edit.
fn recover(root: &Path) -> std::io::Result<Option<String>> {
    let path = root.join(BACKUP);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let bad = |why: &str| std::io::Error::new(ErrorKind::InvalidData, format!("{BACKUP}: {why}"));
    let (rel, original, mutated) =
        parse_backup(&bytes).ok_or_else(|| bad("unreadable; left in place"))?;
    // It sits in the repo tree, so it may not be ours: never follow it out.
    if Path::new(rel)
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(bad("names a path outside the project; left in place"));
    }
    let target = root.join(rel);
    let current = std::fs::read(&target).map_err(|e| {
        bad(&format!(
            "cannot read {rel} to restore it ({e}); left in place"
        ))
    })?;
    if current == mutated {
        replace_file(&target, original)?;
    } else if current != original {
        return Err(bad(&format!(
            "{rel} was edited while a mutant was applied; left it, and its original in the backup"
        )));
    }
    std::fs::remove_file(&path)?;
    Ok(Some(rel.to_string()))
}

fn parse_backup(bytes: &[u8]) -> Option<(&str, &[u8], &[u8])> {
    let (rel, rest) = split_line(bytes)?;
    let (len, body) = split_line(rest)?;
    let len: usize = std::str::from_utf8(len).ok()?.parse().ok()?;
    let rel = std::str::from_utf8(rel).ok()?;
    (len <= body.len() && !rel.is_empty()).then(|| (rel, &body[..len], &body[len..]))
}

fn split_line(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let nl = bytes.iter().position(|&b| b == b'\n')?;
    Some((&bytes[..nl], &bytes[nl + 1..]))
}

/// Session-start recovery (see [`recover`]).
pub(crate) fn recover_at_start(root: &Path) {
    match recover(root) {
        Ok(Some(file)) => {
            tracing::warn!("restored {file} from a mutant left by an interrupted verify run")
        }
        Ok(None) => {}
        Err(e) => tracing::error!("mutation backup recovery: {e}"),
    }
}

/// The affected tests for one file. ponytail: Rust (its crate) and Go (its
/// package) only — other ecosystems' files get no mutants until they have a
/// per-file test mapping.
fn test_cmd_for(root: &Path, rel: &str) -> Option<String> {
    let path = Path::new(rel);
    match path.extension()?.to_str()? {
        "rs" => crate_name_for(root, path).map(|c| format!("cargo test --quiet -p {c}")),
        "go" if root.join("go.mod").exists() => {
            let dir = path.parent()?.to_string_lossy().replace('\\', "/");
            Some(if dir.is_empty() {
                "go test .".into()
            } else {
                format!("go test ./{dir}")
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL: &str = "pub fn eq(a: u32, b: u32) -> bool {\n    a == b\n}\n";

    fn project() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), ORIGINAL).unwrap();
        let mutated = ORIGINAL.replace(" == ", " != ");
        (dir, mutated)
    }

    fn read(root: &Path) -> String {
        std::fs::read_to_string(root.join("src/lib.rs")).unwrap()
    }

    #[test]
    fn picks_operators_on_changed_lines_outside_comments_and_strings() {
        let src = "fn f(a: u32, b: u32) -> bool {\n    // a == b\n    let s = \"x < y\"; let untrue = 1 + 2;\n    a == b && b < 3 || true\n}\n";
        let lines: BTreeSet<u32> = [1, 2, 3, 4].into();
        let spans = wingman_ts::literal_spans(wingman_ts::Language::Rust, src);
        let found: Vec<(u32, &str)> = mutants_in(src, &lines, &spans)
            .iter()
            .map(|m| (m.line, m.from.trim()))
            .collect();
        assert_eq!(
            found,
            [
                (3, "+"),
                (4, "=="),
                (4, "&&"),
                (4, "<"),
                (4, "||"),
                (4, "true")
            ]
        );
        // Unchanged lines yield nothing.
        assert!(mutants_in(src, &[5].into(), &spans).is_empty());

        let m = &mutants_in(src, &[4].into(), &spans)[0];
        assert!(mutate(src, m).contains("    a != b && b < 3 || true\n"));
    }

    #[test]
    fn restores_the_file_when_the_run_panics_mid_mutant() {
        let (dir, mutated) = project();
        let root = dir.path();
        let result = std::panic::catch_unwind(|| {
            let _guard = apply(root, "src/lib.rs", ORIGINAL, &mutated).unwrap();
            assert_eq!(read(root), mutated);
            assert!(root.join(BACKUP).exists());
            panic!("test run blew up");
        });
        assert!(result.is_err());
        assert_eq!(read(root), ORIGINAL);
        assert!(!root.join(BACKUP).exists());
    }

    #[tokio::test]
    async fn restores_the_file_when_the_run_times_out() {
        let (dir, mutated) = project();
        let root = dir.path();
        let run = async {
            let _guard = apply(root, "src/lib.rs", ORIGINAL, &mutated).unwrap();
            std::future::pending::<()>().await;
        };
        let timed_out = tokio::time::timeout(Duration::from_millis(50), run).await;
        assert!(timed_out.is_err());
        assert_eq!(read(root), ORIGINAL);
        assert!(!root.join(BACKUP).exists());
    }

    #[test]
    fn recovers_a_leftover_backup_at_the_next_start() {
        let (dir, mutated) = project();
        let root = dir.path();
        // A killed process never runs the guard.
        std::mem::forget(apply(root, "src/lib.rs", ORIGINAL, &mutated).unwrap());
        assert_eq!(read(root), mutated);

        recover_at_start(root);
        assert_eq!(read(root), ORIGINAL);
        assert!(!root.join(BACKUP).exists());
    }

    #[test]
    fn leaves_a_file_edited_since_the_mutant_alone() {
        let (dir, mutated) = project();
        let root = dir.path();
        std::mem::forget(apply(root, "src/lib.rs", ORIGINAL, &mutated).unwrap());
        std::fs::write(root.join("src/lib.rs"), "// the user's own edit\n").unwrap();

        assert!(recover(root).is_err());
        assert_eq!(read(root), "// the user's own edit\n");
        assert!(root.join(BACKUP).exists(), "the original must survive");
    }

    #[test]
    fn refuses_a_backup_naming_a_path_outside_the_project() {
        let (dir, _) = project();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".wingman")).unwrap();
        std::fs::write(root.join(BACKUP), "../escape.rs\n1\nab").unwrap();
        assert!(recover(root).is_err());
        assert!(root.join(BACKUP).exists());
    }
}
