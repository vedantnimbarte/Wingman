//! J8 — project knowledge graph (durable, beyond per-run session logs).
//!
//! Session logs are turn-by-turn and per-run. This module maintains a
//! project-scoped knowledge layer under `.wingman/knowledge/`:
//!
//! - [`Hotspots`] — files most-edited / most-conflicted across runs,
//!   accumulated in `hotspots.json` from each run's `task.status` (files a
//!   landed attempt changed) and `run.conflict` events. The planner is shown
//!   the most-conflicted files so it declares them in `writes`, which is what
//!   the E4 scheduler serialises on.
//! - [`DecisionRecord`] + [`append_decision`]/[`load_decisions`] — an
//!   append-only `decisions.jsonl` of architectural choices made by
//!   autonomous runs, the latest of which the planner (E2) reads.
//! - [`render_architecture`] — `architecture.md`: a summary the
//!   knowledge-keeper agent keeps current, above a module map regenerated from
//!   the crates' `lib.rs` files after every merged run.
//!
//! [`render_planner_context`] is the read side. The pure pieces (hotspot
//! ranking, architecture rendering, keeper-reply parsing) unit-test without
//! I/O; the `decisions.jsonl` helpers mirror `learning.rs`'s tolerant JSONL
//! pattern.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{Event, TaskStatus};

// ---------------------------------------------------------------------------
// Hotspots
// ---------------------------------------------------------------------------

/// Per-file activity counters used to bias the write-set scheduler (E4):
/// a file that's frequently conflicted should rarely be in the same
/// concurrency wave as anything that touches it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
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

    /// Fold in one run's log: every file a landed attempt (`task.status` to
    /// review) reported changing is an edit, every file in a `run.conflict`
    /// a conflict.
    pub fn observe_run(&mut self, events: &[Event]) {
        for ev in events {
            match ev {
                Event::TaskStatus {
                    status: TaskStatus::Review,
                    outcome: Some(outcome),
                    ..
                } => outcome
                    .files_changed
                    .iter()
                    .for_each(|f| self.record_edit(f)),
                Event::RunConflict { files, .. } => {
                    files.iter().for_each(|f| self.record_conflict(f))
                }
                _ => {}
            }
        }
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

/// `<knowledge_dir>/hotspots.json`.
pub fn hotspots_path(knowledge_dir: &Path) -> PathBuf {
    knowledge_dir.join("hotspots.json")
}

/// The accumulated hotspots, or none when the file is missing or unreadable
/// (a corrupt file restarts the count rather than failing the run).
pub fn load_hotspots(path: &Path) -> Hotspots {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_hotspots(path: &Path, hotspots: &Hotspots) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(hotspots).map_err(io::Error::other)?;
    fs::write(path, body)
}

// ---------------------------------------------------------------------------
// Decisions log
// ---------------------------------------------------------------------------

/// One architectural decision, appended to `.wingman/knowledge/decisions.jsonl`.
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

/// `<project>/.wingman/knowledge/`.
pub fn knowledge_dir(project_root: &Path) -> PathBuf {
    project_root.join(".wingman").join("knowledge")
}

pub fn append_decision(path: &Path, rec: &DecisionRecord) -> io::Result<()> {
    let line = serde_json::to_string(rec).map_err(io::Error::other)?;
    wingman_config::append_line(path, &line)
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

const SUMMARY_START: &str = "<!-- knowledge-keeper summary -->";
const SUMMARY_END: &str = "<!-- /knowledge-keeper summary -->";

/// `<knowledge_dir>/architecture.md`.
pub fn architecture_path(knowledge_dir: &Path) -> PathBuf {
    knowledge_dir.join("architecture.md")
}

/// Render `architecture.md`: the knowledge-keeper's summary, when there is
/// one, above the crate → modules listing. The summary sits between marker
/// comments so [`architecture_summary`] can lift it back out whatever
/// headings it contains.
pub fn render_architecture(summary: Option<&str>, crates: &[(String, Vec<String>)]) -> String {
    let mut out = String::from("# Architecture\n\n");
    out.push_str("_Auto-maintained by pilot mode (J8). Regenerated after every merged run._\n\n");
    if let Some(summary) = summary.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(&format!("{SUMMARY_START}\n{summary}\n{SUMMARY_END}\n\n"));
    }
    out.push_str("## Modules\n\n");
    if crates.is_empty() {
        out.push_str("_No crates discovered._\n");
        return out;
    }
    for (name, modules) in crates {
        out.push_str(&format!("### `{name}`\n\n"));
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

/// The knowledge-keeper summary inside an `architecture.md`, if it has one.
pub fn architecture_summary(md: &str) -> Option<String> {
    let start = md.find(SUMMARY_START)? + SUMMARY_START.len();
    let len = md[start..].find(SUMMARY_END)?;
    Some(md[start..start + len].trim().to_string()).filter(|s| !s.is_empty())
}

/// What the knowledge-keeper agent replies with.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct KeeperReport {
    /// The whole updated architecture summary, in Markdown.
    pub summary: String,
    #[serde(default)]
    pub decisions: Vec<KeeperDecision>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct KeeperDecision {
    pub decision: String,
    #[serde(default)]
    pub rationale: String,
}

/// Parse the keeper's JSON reply. A reply with an empty summary is refused,
/// so a bad answer never blanks the summary that was there.
pub fn parse_keeper_report(json: &str) -> Option<KeeperReport> {
    serde_json::from_str::<KeeperReport>(json)
        .ok()
        .filter(|r| !r.summary.trim().is_empty())
}

/// How many decisions, and how many hot files, the planner is shown.
const PLANNER_DECISIONS: usize = 5;
const PLANNER_HOTSPOTS: usize = 8;
/// Bound on the summary text handed to the planner, in characters.
const PLANNER_SUMMARY_CHARS: usize = 2_000;

/// The knowledge layer as a block for the planner's prompt: the architecture
/// summary, the latest decisions, and the files earlier runs conflicted on.
/// `None` when the project has none of them yet.
pub fn render_planner_context(project_root: &Path) -> Option<String> {
    let dir = knowledge_dir(project_root);
    let mut out = String::new();
    if let Some(summary) = fs::read_to_string(architecture_path(&dir))
        .ok()
        .and_then(|md| architecture_summary(&md))
    {
        let summary: String = summary.chars().take(PLANNER_SUMMARY_CHARS).collect();
        out.push_str(&format!("### Architecture summary\n\n{summary}\n\n"));
    }
    let decisions = load_decisions(&decisions_path(&dir)).unwrap_or_default();
    if !decisions.is_empty() {
        out.push_str("### Recent decisions\n\n");
        for d in decisions.iter().rev().take(PLANNER_DECISIONS) {
            let rationale: String = d.rationale.chars().take(200).collect();
            out.push_str(&format!("- {} — {rationale}\n", d.decision));
        }
        out.push('\n');
    }
    let hotspots = load_hotspots(&hotspots_path(&dir));
    let hot: Vec<(String, u32)> = hotspots
        .ranked()
        .into_iter()
        .filter(|(f, _)| hotspots.conflict_count(f) > 0)
        .take(PLANNER_HOTSPOTS)
        .collect();
    if !hot.is_empty() {
        out.push_str(
            "### Merge hotspots\n\nEarlier runs hit merge conflicts on these files. A task that \
             edits one must list it in `writes`, so no other task touching it runs at the same \
             time:\n\n",
        );
        for (f, _) in hot {
            out.push_str(&format!(
                "- `{f}` ({} conflicts, {} edits)\n",
                hotspots.conflict_count(&f),
                hotspots.edit_count(&f)
            ));
        }
    }
    (!out.is_empty()).then(|| format!("## Project knowledge\n\n{}", out.trim_end()))
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

    fn landed(id: &str, files: &[&str]) -> Event {
        Event::TaskStatus {
            t: "t".into(),
            id: id.into(),
            status: TaskStatus::Review,
            outcome: Some(crate::model::TaskOutcome {
                summary: String::new(),
                files_changed: files.iter().map(|f| f.to_string()).collect(),
            }),
        }
    }

    fn conflict(files: &[&str]) -> Event {
        Event::RunConflict {
            t: "t".into(),
            id: "t2".into(),
            files: files.iter().map(|f| f.to_string()).collect(),
        }
    }

    #[test]
    fn ranked_orders_hottest_first() {
        let mut h = Hotspots::default();
        h.observe_run(&[
            landed("t1", &["cold.rs", "hot.rs", "warm.rs"]),
            landed("t2", &["hot.rs", "warm.rs"]),
            conflict(&["hot.rs"]),
            // A failed attempt's files were never landed, so they don't count.
            Event::TaskStatus {
                t: "t".into(),
                id: "t3".into(),
                status: TaskStatus::Failed,
                outcome: Some(crate::model::TaskOutcome {
                    summary: String::new(),
                    files_changed: vec!["cold.rs".into(); 5],
                }),
            },
        ]);
        let ranked = h.ranked();
        assert_eq!(ranked[0], ("hot.rs".to_string(), 7)); // 2 edits + 1 conflict
        assert_eq!(ranked[1], ("warm.rs".to_string(), 2));
        assert_eq!(ranked[2], ("cold.rs".to_string(), 1));
    }

    #[test]
    fn hotspots_accumulate_across_runs_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = hotspots_path(dir.path());
        assert_eq!(load_hotspots(&path), Hotspots::default());
        for _ in 0..2 {
            let mut h = load_hotspots(&path);
            h.observe_run(&[landed("t1", &["a.rs"]), conflict(&["a.rs"])]);
            save_hotspots(&path, &h).unwrap();
        }
        let h = load_hotspots(&path);
        assert_eq!((h.edit_count("a.rs"), h.conflict_count("a.rs")), (2, 2));
        fs::write(&path, "not json").unwrap();
        assert_eq!(load_hotspots(&path), Hotspots::default());
    }

    #[test]
    fn ranked_breaks_ties_by_name() {
        let mut h = Hotspots::default();
        h.observe_run(&[landed("t1", &["z.rs", "a.rs"])]);
        let ranked = h.ranked();
        // Equal heat → alphabetical.
        assert_eq!(ranked[0].0, "a.rs");
        assert_eq!(ranked[1].0, "z.rs");
    }

    #[test]
    fn decisions_roundtrip() {
        let dir = std::env::temp_dir().join(format!("wingman-know-{}", std::process::id()));
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
                "wingman-autonomous".to_string(),
                vec!["orchestrator".to_string(), "planner".to_string()],
            ),
            ("wingman-cli".to_string(), vec![]),
        ];
        let md = render_architecture(None, &crates);
        assert!(md.contains("# Architecture"));
        assert!(md.contains("### `wingman-autonomous`"));
        assert!(md.contains("- `orchestrator`"));
        assert!(md.contains("no public modules"));
        assert_eq!(architecture_summary(&md), None);
    }

    #[test]
    fn architecture_handles_empty() {
        assert!(render_architecture(None, &[]).contains("No crates discovered"));
    }

    #[test]
    fn architecture_summary_round_trips_through_its_markers() {
        let summary = "## Layers\n\nThe CLI drives the core loop.";
        let md = render_architecture(Some(summary), &[("c".to_string(), vec![])]);
        assert_eq!(architecture_summary(&md).as_deref(), Some(summary));
        assert!(md.find(summary).unwrap() < md.find("## Modules").unwrap());
    }

    #[test]
    fn keeper_report_parses_and_refuses_an_empty_summary() {
        let r = parse_keeper_report(
            r#"{"summary":"s","decisions":[{"decision":"d","rationale":"r"},{"decision":"e"}]}"#,
        )
        .unwrap();
        assert_eq!(r.decisions.len(), 2);
        assert_eq!(r.decisions[1].rationale, "");
        assert!(parse_keeper_report(r#"{"summary":"  ","decisions":[]}"#).is_none());
        assert!(parse_keeper_report("nope").is_none());
    }

    #[test]
    fn planner_context_reads_summary_decisions_and_conflicted_files() {
        let root = tempfile::tempdir().unwrap();
        assert!(render_planner_context(root.path()).is_none());
        let dir = knowledge_dir(root.path());
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            architecture_path(&dir),
            render_architecture(Some("Core owns the loop."), &[]),
        )
        .unwrap();
        for i in 0..7 {
            append_decision(
                &decisions_path(&dir),
                &DecisionRecord {
                    run_id: format!("r{i}"),
                    t: "t".into(),
                    decision: format!("decision {i}"),
                    rationale: "why".into(),
                },
            )
            .unwrap();
        }
        let mut h = Hotspots::default();
        h.observe_run(&[
            landed("t1", &["edited.rs", "fought.rs"]),
            conflict(&["fought.rs"]),
        ]);
        save_hotspots(&hotspots_path(&dir), &h).unwrap();

        let ctx = render_planner_context(root.path()).unwrap();
        assert!(ctx.contains("Core owns the loop."));
        // The latest five decisions, newest first.
        assert!(ctx.contains("decision 6") && ctx.contains("decision 2"));
        assert!(!ctx.contains("decision 1"));
        assert!(ctx.contains("`fought.rs` (1 conflicts, 1 edits)"));
        assert!(
            !ctx.contains("edited.rs"),
            "files never conflicted on are left out"
        );
    }
}
