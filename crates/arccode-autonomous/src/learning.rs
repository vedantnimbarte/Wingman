//! E6 — cross-run learning loop + adaptive model routing.
//!
//! Three mechanisms, all reading/writing under `~/.arccode/`:
//!
//! 1. **Per-`(role, model)` stats** in `~/.arccode/stats.jsonl` — one
//!    append-only [`StatRecord`] per task attempt, carrying the first-try
//!    pass/fail and (later, via R2) the PR outcome. [`aggregate`] folds
//!    these into [`Aggregates`]; [`pick_model`] uses them for adaptive
//!    routing.
//! 2. **Per-role lessons** in `~/.arccode/agents/<role>.lessons.md` —
//!    appended whenever a task by that role is reverted or heavily
//!    rewritten; loaded into the role's system prompt on later runs.
//! 3. **Planner priming** — [`rank_similar_runs`] surfaces the top-K most
//!    similar past goals (lexical Jaccard, no embeddings dependency) so
//!    E2's draft pass can condition on what worked before.
//!
//! The blended success rate combines the cheap-but-shallow first-try
//! signal with R2's expensive-but-true post-merge signal: a model that
//! passes acceptance on the first try but whose PRs keep getting reverted
//! is *worse* than its first-try rate suggests, and the router must see
//! that.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::feedback::WeightedStats;
use crate::model::PrOutcomeKind;

/// One append-only line in `~/.arccode/stats.jsonl`. Written once when a
/// task attempt resolves (first-try outcome known) and amended via a
/// fresh record once the PR outcome is observed (R2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatRecord {
    pub run_id: String,
    /// `Role::as_str()` — kept as a string so skill-pack custom roles
    /// round-trip without a model change.
    pub role: String,
    /// `provider/model_id` of the model that executed the attempt.
    pub model: String,
    /// Coarse task category for finer routing (e.g. "edit", "refactor").
    #[serde(default)]
    pub task_kind: Option<String>,
    /// Did the task pass acceptance on the first worker turn?
    pub first_try_ok: bool,
    /// Post-merge outcome, once R2's poller observes it. `None` until then.
    #[serde(default)]
    pub pr_outcome: Option<PrOutcomeKind>,
    /// Original goal text — used for similarity priming.
    #[serde(default)]
    pub goal: String,
    /// RFC-3339 timestamp.
    #[serde(default)]
    pub t: String,
}

/// Aggregated counters for one `(role, model)` bucket.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bucket {
    pub attempts: u32,
    pub first_try_successes: u32,
    pub post_merge: WeightedStats,
}

impl Bucket {
    /// First-try pass rate, or `None` with no attempts.
    pub fn first_try_rate(&self) -> Option<f64> {
        if self.attempts == 0 {
            None
        } else {
            Some(self.first_try_successes as f64 / self.attempts as f64)
        }
    }

    /// Blended success rate: 30% first-try, 70% post-merge adjusted rate
    /// when post-merge data exists; pure first-try otherwise; `None` when
    /// there's no data at all.
    ///
    /// The post-merge weight dominates because R2's signal reflects
    /// production reality, while first-try only reflects what survived
    /// acceptance.
    pub fn blended_rate(&self) -> Option<f64> {
        match (
            self.first_try_rate(),
            self.post_merge.adjusted_success_rate(),
        ) {
            (Some(ft), Some(pm)) => Some(0.3 * ft + 0.7 * pm),
            (Some(ft), None) => Some(ft),
            (None, Some(pm)) => Some(pm),
            (None, None) => None,
        }
    }
}

/// All buckets, keyed by `(role, model)`.
#[derive(Debug, Clone, Default)]
pub struct Aggregates {
    pub buckets: BTreeMap<(String, String), Bucket>,
}

impl Aggregates {
    pub fn bucket(&self, role: &str, model: &str) -> Option<&Bucket> {
        self.buckets.get(&(role.to_string(), model.to_string()))
    }
}

/// Fold a stream of [`StatRecord`] into per-`(role, model)` [`Bucket`]s.
pub fn aggregate(records: impl IntoIterator<Item = StatRecord>) -> Aggregates {
    let mut agg = Aggregates::default();
    for r in records {
        let b = agg
            .buckets
            .entry((r.role.clone(), r.model.clone()))
            .or_default();
        b.attempts += 1;
        if r.first_try_ok {
            b.first_try_successes += 1;
        }
        if let Some(kind) = r.pr_outcome {
            b.post_merge.record(kind);
        }
    }
    agg
}

/// Adaptive model routing (E6 §3). Given `candidates` ordered
/// cheapest-first, return the cheapest model whose blended success rate
/// for `role` clears `threshold`.
///
/// Exploration: a candidate with fewer than `min_samples` attempts is
/// given the benefit of the doubt (treated as passing) so cheap,
/// untried models get sampled rather than being starved by the
/// incumbent. When no candidate qualifies, fall back to the last
/// (most-capable) candidate.
pub fn pick_model<'a>(
    candidates: &'a [String],
    agg: &Aggregates,
    role: &str,
    threshold: f64,
    min_samples: u32,
) -> Option<&'a str> {
    if candidates.is_empty() {
        return None;
    }
    for model in candidates {
        match agg.bucket(role, model) {
            // Under-sampled: explore it.
            Some(b) if b.attempts < min_samples => return Some(model.as_str()),
            None => return Some(model.as_str()),
            Some(b) => {
                if b.blended_rate().is_some_and(|r| r >= threshold) {
                    return Some(model.as_str());
                }
            }
        }
    }
    // Nobody cleared the bar — use the most capable (last) candidate.
    candidates.last().map(String::as_str)
}

/// E6 convenience wrapper used by the live worker spawner: choose a worker
/// model for `role`, preferring the cheaper `cheap` model but escalating to
/// the more capable `capable` model when `cheap`'s blended success rate for
/// this role is below `threshold` (after at least `min_samples` attempts).
///
/// With no history, returns `cheap` (explore the cheap model first). When
/// `cheap == capable` (no distinct worker model configured) there's only
/// one candidate and the result is always that model.
pub fn route_model(
    agg: &Aggregates,
    role: &str,
    cheap: &str,
    capable: &str,
    threshold: f64,
    min_samples: u32,
) -> String {
    let candidates = if cheap == capable {
        vec![cheap.to_string()]
    } else {
        vec![cheap.to_string(), capable.to_string()]
    };
    pick_model(&candidates, agg, role, threshold, min_samples)
        .map(str::to_string)
        .unwrap_or_else(|| cheap.to_string())
}

// ---------------------------------------------------------------------------
// stats.jsonl I/O
// ---------------------------------------------------------------------------

/// Conventional stats path: `<home>/.arccode/stats.jsonl`.
pub fn stats_path(home: &Path) -> PathBuf {
    home.join(".arccode").join("stats.jsonl")
}

/// Append one record. Creates the file (and parent dir) if missing.
pub fn append_stat(path: &Path, rec: &StatRecord) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(rec).map_err(io::Error::other)?;
    writeln!(f, "{line}")
}

/// Load all records, tolerating blank/corrupt lines (skips them). Returns
/// an empty vec if the file doesn't exist.
pub fn load_stats(path: &Path) -> io::Result<Vec<StatRecord>> {
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
        if let Ok(rec) = serde_json::from_str::<StatRecord>(&line) {
            out.push(rec);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Per-role lessons
// ---------------------------------------------------------------------------

/// A single learned lesson, appended to a role's lessons file on a revert
/// or heavy rewrite.
#[derive(Debug, Clone, PartialEq)]
pub struct Lesson {
    pub run_id: String,
    pub t: String,
    /// What went wrong / what to do differently.
    pub text: String,
}

/// Conventional lessons path: `<home>/.arccode/agents/<role>.lessons.md`.
pub fn lessons_path(home: &Path, role: &str) -> PathBuf {
    home.join(".arccode")
        .join("agents")
        .join(format!("{role}.lessons.md"))
}

/// Append a lesson as a markdown bullet. Seeds a heading on first write.
pub fn append_lesson(path: &Path, lesson: &Lesson) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let fresh = !path.exists();
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    if fresh {
        writeln!(f, "# Lessons\n")?;
        writeln!(
            f,
            "Auto-maintained by pilot mode (E6). Each entry is a takeaway from a\nreverted or heavily-rewritten task by this role.\n"
        )?;
    }
    writeln!(
        f,
        "- ({}) {} — run `{}`",
        lesson.t, lesson.text, lesson.run_id
    )
}

/// Load a role's accumulated lessons (the raw markdown body), or `None`
/// when the file is missing or has no content beyond whitespace. The
/// counterpart to [`append_lesson`]: this is what later runs read back to
/// fold prior mistakes into the role's system prompt.
pub fn load_lessons(path: &Path) -> io::Result<Option<String>> {
    let body = match fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if body.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(body))
    }
}

/// Render a role's lessons as a system-prompt appendix the worker should
/// heed, or `None` when there are no lessons. Pure: takes the already-read
/// lessons body so it's testable without a filesystem.
///
/// The wording frames the lessons as hard-won constraints from prior
/// reverted/rewritten work, not optional suggestions — a worker that
/// ignores them tends to reproduce the exact failure that generated them.
pub fn render_lessons_appendix(lessons_body: &str) -> Option<String> {
    let trimmed = lessons_body.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(format!(
        "\n\n---\n\nLESSONS FROM PRIOR RUNS BY THIS ROLE (these are constraints learned the \
         hard way from reverted or heavily-rewritten work — honour them unless the task \
         explicitly overrides):\n\n{trimmed}"
    ))
}

// ---------------------------------------------------------------------------
// Planner priming: lexical similarity over goals
// ---------------------------------------------------------------------------

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "add", "fix", "make", "use",
    "when", "then", "than", "but", "not", "all", "any", "are", "was", "has",
];

/// Tokenise `text` into a set of lowercase alphanumeric words longer than
/// three characters, minus common stopwords. Used for Jaccard similarity.
pub fn keyword_set(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 3)
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// Jaccard similarity of two keyword sets: `|A ∩ B| / |A ∪ B|`. Two empty
/// sets are defined as similarity 0 (no signal).
pub fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Rank `past` records by goal-similarity to `goal`, returning the top-`k`
/// (most similar first). Records with zero similarity are dropped. Ties
/// break toward more recent records (later in the input order wins on a
/// stable sort by negated index — but we keep it simple and stable).
pub fn rank_similar_runs<'a>(goal: &str, past: &'a [StatRecord], k: usize) -> Vec<&'a StatRecord> {
    let target = keyword_set(goal);
    let mut scored: Vec<(f64, &StatRecord)> = past
        .iter()
        .map(|r| (jaccard(&target, &keyword_set(&r.goal)), r))
        .filter(|(s, _)| *s > 0.0)
        .collect();
    // Higher score first; partial_cmp is safe since scores are finite.
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(k).map(|(_, r)| r).collect()
}

/// E6 planner priming: render the top-`k` most similar past runs (by goal
/// similarity) with their observed outcomes as an in-context block the
/// planner can condition on before its draft pass. Dedupes by `run_id`
/// (stats are per-task, so one run yields many records) and biases the
/// planner toward approaches that merged and away from ones that were
/// reverted. Returns `None` when nothing is similar enough.
pub fn render_priming(goal: &str, past: &[StatRecord], k: usize) -> Option<String> {
    if k == 0 {
        return None;
    }
    // Over-fetch: rank_similar_runs returns per-task records, so we need
    // more than `k` to end up with `k` distinct runs after deduping.
    let similar = rank_similar_runs(goal, past, k.saturating_mul(4).max(8));
    let mut seen = BTreeSet::new();
    let mut lines = Vec::new();
    for r in similar {
        if r.goal.trim().is_empty() || !seen.insert(r.run_id.clone()) {
            continue;
        }
        let outcome = match r.pr_outcome {
            Some(PrOutcomeKind::Merged) => "merged (a good sign)",
            Some(PrOutcomeKind::Reverted) => "reverted — avoid repeating that approach",
            Some(PrOutcomeKind::HotfixFollowed) => "merged but needed a hotfix — be careful",
            Some(PrOutcomeKind::Closed) => "closed without merging",
            None if r.first_try_ok => "completed cleanly",
            None => "struggled (took retries)",
        };
        lines.push(format!("- \"{}\" → {}", r.goal.trim(), outcome));
        if lines.len() >= k {
            break;
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(format!(
            "PAST SIMILAR RUNS (learn from these outcomes — prefer approaches that merged, \
             avoid ones that were reverted):\n{}",
            lines.join("\n")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(
        run: &str,
        role: &str,
        model: &str,
        ok: bool,
        outcome: Option<PrOutcomeKind>,
    ) -> StatRecord {
        StatRecord {
            run_id: run.into(),
            role: role.into(),
            model: model.into(),
            task_kind: None,
            first_try_ok: ok,
            pr_outcome: outcome,
            goal: String::new(),
            t: String::new(),
        }
    }

    #[test]
    fn aggregate_counts_first_try_and_outcomes() {
        let records = vec![
            rec(
                "r1",
                "developer",
                "haiku",
                true,
                Some(PrOutcomeKind::Merged),
            ),
            rec(
                "r2",
                "developer",
                "haiku",
                false,
                Some(PrOutcomeKind::Reverted),
            ),
            rec("r3", "developer", "haiku", true, None),
        ];
        let agg = aggregate(records);
        let b = agg.bucket("developer", "haiku").unwrap();
        assert_eq!(b.attempts, 3);
        assert_eq!(b.first_try_successes, 2);
        assert_eq!(b.post_merge.merged, 1);
        assert_eq!(b.post_merge.reverted, 1);
    }

    #[test]
    fn blended_rate_falls_back_to_first_try_without_post_merge() {
        let b = Bucket {
            attempts: 4,
            first_try_successes: 3,
            ..Default::default()
        };
        assert_eq!(b.blended_rate(), Some(0.75));
    }

    #[test]
    fn blended_rate_weights_post_merge_more() {
        // First-try perfect but every PR reverted → blended should be
        // dragged down hard.
        let b = Bucket {
            attempts: 2,
            first_try_successes: 2, // first_try_rate = 1.0
            post_merge: WeightedStats {
                reverted: 2, // adjusted_rate = 0.0
                ..Default::default()
            },
        };
        // 0.3*1.0 + 0.7*0.0 = 0.3
        assert!((b.blended_rate().unwrap() - 0.3).abs() < 1e-9);
    }

    #[test]
    fn pick_model_explores_untried_cheap_candidate() {
        let candidates = vec!["cheap".to_string(), "pricey".to_string()];
        let agg = Aggregates::default();
        // No data at all → explore the cheapest.
        assert_eq!(
            pick_model(&candidates, &agg, "developer", 0.8, 3),
            Some("cheap")
        );
    }

    #[test]
    fn pick_model_skips_proven_bad_cheap_model() {
        let candidates = vec!["cheap".to_string(), "pricey".to_string()];
        let records = vec![
            rec(
                "r1",
                "developer",
                "cheap",
                false,
                Some(PrOutcomeKind::Reverted),
            ),
            rec(
                "r2",
                "developer",
                "cheap",
                false,
                Some(PrOutcomeKind::Reverted),
            ),
            rec(
                "r3",
                "developer",
                "cheap",
                false,
                Some(PrOutcomeKind::Reverted),
            ),
            // pricey is untried → benefit of the doubt.
        ];
        let agg = aggregate(records);
        assert_eq!(
            pick_model(&candidates, &agg, "developer", 0.8, 3),
            Some("pricey")
        );
    }

    #[test]
    fn pick_model_falls_back_to_most_capable_when_none_qualify() {
        let candidates = vec!["cheap".to_string(), "pricey".to_string()];
        let records = vec![
            rec("r1", "developer", "cheap", false, None),
            rec("r2", "developer", "cheap", false, None),
            rec("r3", "developer", "cheap", false, None),
            rec("r4", "developer", "pricey", false, None),
            rec("r5", "developer", "pricey", false, None),
            rec("r6", "developer", "pricey", false, None),
        ];
        let agg = aggregate(records);
        // Both proven bad (rate 0 < 0.8) and both have >= min_samples →
        // fall back to last candidate.
        assert_eq!(
            pick_model(&candidates, &agg, "developer", 0.8, 3),
            Some("pricey")
        );
    }

    #[test]
    fn pick_model_empty_candidates_is_none() {
        assert_eq!(pick_model(&[], &Aggregates::default(), "x", 0.5, 1), None);
    }

    #[test]
    fn route_model_explores_cheap_without_history() {
        let agg = Aggregates::default();
        assert_eq!(
            route_model(&agg, "developer", "haiku", "opus", 0.7, 3),
            "haiku"
        );
    }

    #[test]
    fn route_model_escalates_when_cheap_proven_bad() {
        // Cheap model fails every first try with enough samples → escalate.
        let records = vec![
            rec("r1", "developer", "haiku", false, None),
            rec("r2", "developer", "haiku", false, None),
            rec("r3", "developer", "haiku", false, None),
        ];
        let agg = aggregate(records);
        assert_eq!(
            route_model(&agg, "developer", "haiku", "opus", 0.7, 3),
            "opus"
        );
    }

    #[test]
    fn route_model_keeps_cheap_when_proven_good() {
        let records = vec![
            rec("r1", "developer", "haiku", true, None),
            rec("r2", "developer", "haiku", true, None),
            rec("r3", "developer", "haiku", true, None),
        ];
        let agg = aggregate(records);
        assert_eq!(
            route_model(&agg, "developer", "haiku", "opus", 0.7, 3),
            "haiku"
        );
    }

    #[test]
    fn route_model_single_candidate_when_cheap_equals_capable() {
        let agg = Aggregates::default();
        assert_eq!(
            route_model(&agg, "developer", "opus", "opus", 0.7, 3),
            "opus"
        );
    }

    #[test]
    fn keyword_set_drops_short_words_and_stopwords() {
        let kw = keyword_set("Add a dark-mode toggle to the TUI composer");
        assert!(kw.contains("dark"));
        assert!(kw.contains("mode"));
        assert!(kw.contains("toggle"));
        assert!(kw.contains("composer"));
        assert!(!kw.contains("add")); // stopword
        assert!(!kw.contains("the")); // stopword
        assert!(!kw.contains("tui")); // too short (3 chars)
    }

    #[test]
    fn jaccard_identical_is_one() {
        let a = keyword_set("refactor the parser module");
        let b = keyword_set("refactor parser module");
        assert!(jaccard(&a, &b) > 0.5);
    }

    #[test]
    fn jaccard_disjoint_is_zero() {
        let a = keyword_set("database migration schema");
        let b = keyword_set("frontend button styling");
        assert_eq!(jaccard(&a, &b), 0.0);
    }

    #[test]
    fn rank_similar_runs_orders_by_overlap() {
        let past = vec![
            StatRecord {
                goal: "add dark mode toggle to settings".into(),
                ..rec("r1", "developer", "m", true, None)
            },
            StatRecord {
                goal: "migrate database schema for users".into(),
                ..rec("r2", "developer", "m", true, None)
            },
            StatRecord {
                goal: "add light mode toggle to settings panel".into(),
                ..rec("r3", "developer", "m", true, None)
            },
        ];
        let ranked = rank_similar_runs("add a mode toggle to the settings", &past, 2);
        assert_eq!(ranked.len(), 2);
        // r1 and r3 both share "mode toggle settings"; r2 (database) is dropped.
        assert!(ranked.iter().all(|r| r.run_id != "r2"));
    }

    #[test]
    fn render_priming_biases_toward_merged_and_against_reverted() {
        let past = vec![
            StatRecord {
                goal: "add dark mode toggle to settings".into(),
                ..rec("r1", "developer", "m", true, Some(PrOutcomeKind::Merged))
            },
            StatRecord {
                goal: "add light mode toggle to settings panel".into(),
                ..rec("r2", "developer", "m", false, Some(PrOutcomeKind::Reverted))
            },
            StatRecord {
                goal: "migrate database schema for billing".into(),
                ..rec("r3", "developer", "m", true, Some(PrOutcomeKind::Merged))
            },
        ];
        let block = render_priming("add a mode toggle to settings", &past, 5).unwrap();
        assert!(block.contains("dark mode toggle"));
        assert!(block.contains("merged"));
        assert!(block.contains("light mode toggle"));
        assert!(block.contains("avoid"));
        // The unrelated database run is dropped (zero similarity).
        assert!(!block.contains("billing"));
    }

    #[test]
    fn render_priming_dedupes_by_run_id() {
        // Two per-task records from the same run + goal → one line.
        let past = vec![
            StatRecord {
                goal: "add export button".into(),
                ..rec("r1", "developer", "m", true, Some(PrOutcomeKind::Merged))
            },
            StatRecord {
                goal: "add export button".into(),
                ..rec("r1", "tester", "m", true, Some(PrOutcomeKind::Merged))
            },
        ];
        let block = render_priming("add export button to toolbar", &past, 5).unwrap();
        assert_eq!(block.matches("export button").count(), 1);
    }

    #[test]
    fn render_priming_none_when_nothing_similar() {
        let past = vec![StatRecord {
            goal: "unrelated database migration".into(),
            ..rec("r1", "developer", "m", true, None)
        }];
        assert!(render_priming("style the landing page header", &past, 5).is_none());
    }

    #[test]
    fn stats_roundtrip_through_file() {
        let dir = std::env::temp_dir().join(format!("arccode-learn-{}", std::process::id()));
        let path = stats_path(&dir);
        let _ = fs::remove_file(&path);
        let r = rec(
            "r1",
            "developer",
            "haiku",
            true,
            Some(PrOutcomeKind::Merged),
        );
        append_stat(&path, &r).unwrap();
        append_stat(&path, &rec("r2", "tester", "haiku", false, None)).unwrap();
        let loaded = load_stats(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0], r);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_stats_missing_file_is_empty() {
        let path = std::env::temp_dir().join("definitely-not-here-arccode.jsonl");
        let _ = fs::remove_file(&path);
        assert!(load_stats(&path).unwrap().is_empty());
    }

    #[test]
    fn load_stats_skips_corrupt_lines() {
        let dir =
            std::env::temp_dir().join(format!("arccode-learn-corrupt-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stats.jsonl");
        fs::write(&path, "{not json}\n{\"run_id\":\"r1\",\"role\":\"developer\",\"model\":\"m\",\"first_try_ok\":true}\n").unwrap();
        let loaded = load_stats(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].run_id, "r1");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_lesson_seeds_heading_then_appends() {
        let dir = std::env::temp_dir().join(format!("arccode-lessons-{}", std::process::id()));
        let path = lessons_path(&dir, "developer");
        let _ = fs::remove_file(&path);
        append_lesson(
            &path,
            &Lesson {
                run_id: "r1".into(),
                t: "2026-05-29".into(),
                text: "avoid unwrap in tools".into(),
            },
        )
        .unwrap();
        append_lesson(
            &path,
            &Lesson {
                run_id: "r2".into(),
                t: "2026-05-30".into(),
                text: "prefer anyhow".into(),
            },
        )
        .unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.starts_with("# Lessons"));
        assert!(body.contains("avoid unwrap in tools"));
        assert!(body.contains("prefer anyhow"));
        // Heading only once.
        assert_eq!(body.matches("# Lessons").count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_lessons_roundtrips_what_append_wrote() {
        let dir = std::env::temp_dir().join(format!("arccode-lessons-load-{}", std::process::id()));
        let path = lessons_path(&dir, "tester");
        let _ = fs::remove_file(&path);
        // Missing file → None.
        assert!(load_lessons(&path).unwrap().is_none());
        append_lesson(
            &path,
            &Lesson {
                run_id: "r1".into(),
                t: "2026-06-02".into(),
                text: "always assert on stderr too".into(),
            },
        )
        .unwrap();
        let body = load_lessons(&path).unwrap().expect("lessons present");
        assert!(body.contains("always assert on stderr too"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_lessons_appendix_wraps_body_or_returns_none() {
        assert!(render_lessons_appendix("   \n  ").is_none());
        let appendix = render_lessons_appendix("- avoid unwrap in tools").unwrap();
        assert!(appendix.contains("LESSONS FROM PRIOR RUNS"));
        assert!(appendix.contains("avoid unwrap in tools"));
        // Starts with a separator so it appends cleanly onto a base prompt.
        assert!(appendix.starts_with("\n\n---"));
    }
}
