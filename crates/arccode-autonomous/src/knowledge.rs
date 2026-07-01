//! J8 — project knowledge graph (durable, beyond per-run session logs).
//!
//! Session logs are turn-by-turn and per-run. This module maintains a
//! project-scoped knowledge layer under `.arccode/knowledge/`:
//!
//! - [`Hotspots`] — files most-edited / most-conflicted across runs,
//!   computed from the `task.commit` / `run.merge.task` event stream and
//!   fed to the E4 scheduler to bias conflict avoidance.
//! - [`DecisionRecord`] + [`append_decision`]/[`load_decisions`] — an
//!   append-only `decisions.jsonl` of architectural choices made by
//!   autonomous runs (and, via R2, of reverts/hotfixes), read by the
//!   planner (E2) and clarify pass (J1) before generating anything.
//! - [`render_architecture`] — a module map regenerated when crate
//!   `lib.rs` files change.
//!
//! The pure pieces (hotspot ranking, architecture rendering) unit-test
//! without I/O; the `decisions.jsonl` helpers mirror `learning.rs`'s
//! tolerant JSONL pattern.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Hotspots
// ---------------------------------------------------------------------------

/// Per-file activity counters used to bias the write-set scheduler (E4):
/// a file that's frequently conflicted should rarely be in the same
/// concurrency wave as anything that touches it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hotspots {
    edits: HashMap<String, u32>,
    conflicts: HashMap<String, u32>,
}

impl Hotspots {
    pub fn record_edit(&mut self, file: &str) {
        *self.edits.entry(file.to_string()).or_insert(0) += 1;
    }

    pub fn record_conflict(&mut self, file: &str) {
        *self.conflicts.entry(file.to_string()).or_insert(0) += 1;
    }

    pub fn edit_count(&self, file: &str) -> u32 {
        self.edits.get(file).copied().unwrap_or(0)
    }

    pub fn conflict_count(&self, file: &str) -> u32 {
        self.conflicts.get(file).copied().unwrap_or(0)
    }

    /// A heat score combining edits and conflicts; conflicts weigh 5×
    /// because they're the signal the scheduler actually cares about.
    pub fn heat(&self, file: &str) -> u32 {
        self.edit_count(file) + 5 * self.conflict_count(file)
    }

    /// Files ranked hottest-first (ties broken by name for determinism).
    pub fn ranked(&self) -> Vec<(String, u32)> {
        let mut all: std::collections::BTreeSet<&String> = self.edits.keys().collect();
        all.extend(self.conflicts.keys());
        let mut v: Vec<(String, u32)> =
            all.into_iter().map(|f| (f.clone(), self.heat(f))).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }
}

/// Build hotspots by folding edit signals. `edits` is a list of
/// `(file, was_conflict)` observations gathered from run history.
pub fn hotspots_from_observations(edits: &[(String, bool)]) -> Hotspots {
    let mut h = Hotspots::default();
    for (file, conflict) in edits {
        h.record_edit(file);
        if *conflict {
            h.record_conflict(file);
        }
    }
    h
}

// ---------------------------------------------------------------------------
// Decisions log
// ---------------------------------------------------------------------------

/// One architectural decision, appended to `.arccode/knowledge/decisions.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub run_id: String,
    pub t: String,
    /// Short statement of what was decided.
    pub decision: String,
    /// Why — extracted from a revert message, critic finding, or run log.
    pub rationale: String,
}

/// `<knowledge_dir>/decisions.jsonl`.
pub fn decisions_path(knowledge_dir: &Path) -> PathBuf {
    knowledge_dir.join("decisions.jsonl")
}

/// `<project>/.arccode/knowledge/`.
pub fn knowledge_dir(project_root: &Path) -> PathBuf {
    project_root.join(".arccode").join("knowledge")
}

pub fn append_decision(path: &Path, rec: &DecisionRecord) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(rec).map_err(io::Error::other)?;
    writeln!(f, "{line}")
}

pub fn load_decisions(path: &Path) -> io::Result<Vec<DecisionRecord>> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for line in io::BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<DecisionRecord>(&line) {
            out.push(rec);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Architecture map
// ---------------------------------------------------------------------------

/// Render `architecture.md` from a crate → modules listing.
pub fn render_architecture(crates: &[(String, Vec<String>)]) -> String {
    let mut out = String::from("# Architecture\n\n");
    out.push_str(
        "_Auto-maintained by pilot mode (J8). Regenerated when crate `lib.rs` files change._\n\n",
    );
    if crates.is_empty() {
        out.push_str("_No crates discovered._\n");
        return out;
    }
    for (name, modules) in crates {
        out.push_str(&format!("## `{name}`\n\n"));
        if modules.is_empty() {
            out.push_str("_(no public modules)_\n\n");
        } else {
            for m in modules {
                out.push_str(&format!("- `{m}`\n"));
            }
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heat_weights_conflicts_more() {
        let mut h = Hotspots::default();
        h.record_edit("a.rs");
        h.record_edit("a.rs"); // 2 edits
        h.record_edit("b.rs");
        h.record_conflict("b.rs"); // 1 edit + 1 conflict
                                   // a: 2; b: 1 + 5 = 6
        assert_eq!(h.heat("a.rs"), 2);
        assert_eq!(h.heat("b.rs"), 6);
    }

    #[test]
    fn ranked_orders_hottest_first() {
        let obs = vec![
            ("cold.rs".to_string(), false),
            ("hot.rs".to_string(), true),
            ("hot.rs".to_string(), true),
            ("warm.rs".to_string(), false),
            ("warm.rs".to_string(), false),
        ];
        let h = hotspots_from_observations(&obs);
        let ranked = h.ranked();
        assert_eq!(ranked[0].0, "hot.rs"); // 2 edits + 2 conflicts = 12
                                           // warm (2 edits = 2) beats cold (1 edit = 1)
        assert_eq!(ranked[1].0, "warm.rs");
        assert_eq!(ranked[2].0, "cold.rs");
    }

    #[test]
    fn ranked_breaks_ties_by_name() {
        let obs = vec![("z.rs".to_string(), false), ("a.rs".to_string(), false)];
        let h = hotspots_from_observations(&obs);
        let ranked = h.ranked();
        // Equal heat → alphabetical.
        assert_eq!(ranked[0].0, "a.rs");
        assert_eq!(ranked[1].0, "z.rs");
    }

    #[test]
    fn decisions_roundtrip() {
        let dir = std::env::temp_dir().join(format!("arccode-know-{}", std::process::id()));
        let path = decisions_path(&dir);
        let _ = fs::remove_file(&path);
        let r = DecisionRecord {
            run_id: "r1".into(),
            t: "2026-05-29".into(),
            decision: "squash-merge per task".into(),
            rationale: "rebase-as-you-go caused 3 conflicts in run X".into(),
        };
        append_decision(&path, &r).unwrap();
        let loaded = load_decisions(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], r);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_decisions_missing_is_empty() {
        let path = std::env::temp_dir().join("no-such-decisions.jsonl");
        let _ = fs::remove_file(&path);
        assert!(load_decisions(&path).unwrap().is_empty());
    }

    #[test]
    fn architecture_renders_crates_and_modules() {
        let crates = vec![
            (
                "arccode-autonomous".to_string(),
                vec!["orchestrator".to_string(), "planner".to_string()],
            ),
            ("arccode-cli".to_string(), vec![]),
        ];
        let md = render_architecture(&crates);
        assert!(md.contains("# Architecture"));
        assert!(md.contains("## `arccode-autonomous`"));
        assert!(md.contains("- `orchestrator`"));
        assert!(md.contains("no public modules"));
    }

    #[test]
    fn architecture_handles_empty() {
        assert!(render_architecture(&[]).contains("No crates discovered"));
    }
}
