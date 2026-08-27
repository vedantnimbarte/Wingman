//! The skill-stats seam.
//!
//! [`StatsStore`] is SQLite at `~/.wingman/learn.db`, and everything in the
//! learning loop reached it concretely. That makes the loop's own logic
//! awkward to test: scoring a skill outcome, or checking that an explicit
//! rating survives the heuristic, has nothing to do with SQLite, but exercising
//! it meant opening a database.
//!
//! [`SkillStats`] is the seam, with two implementations: the SQLite store, and
//! [`MemoryStats`], which holds the same records in a `Vec`.
//!
//! ## The cost, and how it is paid
//!
//! Two implementations of the same rules can drift, and these rules are not
//! trivial — an inferred outcome must not overwrite a stated one, feedback
//! attaches to the most recent *unrated* invocation inside a time window, and
//! the summary counts stated and inferred outcomes separately.
//!
//! That is exactly why [`tests`] runs one conformance script against both. A
//! rule implemented in only one of them fails there rather than in production
//! six months later, which makes the duplication safe rather than merely
//! cheaper than SQLite.
//!
//! ## What is in, and what is not
//!
//! Every method here has a caller in [`crate::hooks::LearnHook`] or the
//! `invoke_skill` tool. Counters are included for that reason — the
//! quiet-session nudge drives them, so testing the nudge without a database
//! needs them — not because a stats trait ought to have counters.
//!
//! Routing outcomes and `record_manual` are **not** here. They are reached
//! through a concrete `StatsStore` by callers that never see this trait, and
//! adding them to round the interface out would be the speculative version of
//! this ([0013](../../../docs/decisions/0013-no-speculative-seams.md)).

use std::sync::Mutex;

use chrono::{Duration, Utc};

use crate::stats::{FeedbackApplied, Outcome, Rating, SkillSummary, SkillUsageRow, StatsStore};
use crate::Result;

/// Recording what skills did and what the user thought of it.
pub trait SkillStats: Send + Sync {
    /// Record a fresh invocation, returning its row id.
    fn record_invoke(&self, skill_name: &str, session_id: &str) -> Result<i64>;

    /// Set an outcome the heuristic inferred. Must not overwrite one the user
    /// stated.
    fn set_outcome(&self, id: i64, outcome: Outcome, signal: Option<&str>) -> Result<()>;

    /// Set an outcome the user stated outright.
    fn set_outcome_explicit(&self, id: i64, outcome: Outcome, note: Option<&str>) -> Result<()>;

    /// Attach a rating to the most recent invocation not already rated,
    /// within `within`.
    fn apply_feedback(
        &self,
        session_id: &str,
        rating: Rating,
        note: Option<&str>,
        within: Duration,
    ) -> Result<FeedbackApplied>;

    /// Per-skill counts, with stated outcomes reported separately.
    fn summary(&self) -> Result<Vec<SkillSummary>>;

    /// Most recent rows for one skill, newest first.
    fn recent(&self, skill_name: &str, limit: usize) -> Result<Vec<SkillUsageRow>>;

    /// How many ratings have been given in total.
    fn feedback_count(&self) -> Result<u32>;

    // Counters. In the trait because `LearnHook` drives the quiet-session
    // nudge with them, so testing that nudge without a database needs them
    // here - a real caller, not a speculative one.

    /// Read a counter, defaulting to zero.
    fn counter_get(&self, key: &str) -> Result<i64>;
    /// Set a counter outright.
    fn counter_set(&self, key: &str, value: i64) -> Result<()>;
    /// Add one and return the new value.
    fn counter_incr(&self, key: &str) -> Result<i64>;
}

impl SkillStats for StatsStore {
    fn record_invoke(&self, skill_name: &str, session_id: &str) -> Result<i64> {
        StatsStore::record_invoke(self, skill_name, session_id)
    }
    fn set_outcome(&self, id: i64, outcome: Outcome, signal: Option<&str>) -> Result<()> {
        StatsStore::set_outcome(self, id, outcome, signal)
    }
    fn set_outcome_explicit(&self, id: i64, outcome: Outcome, note: Option<&str>) -> Result<()> {
        StatsStore::set_outcome_explicit(self, id, outcome, note)
    }
    fn apply_feedback(
        &self,
        session_id: &str,
        rating: Rating,
        note: Option<&str>,
        within: Duration,
    ) -> Result<FeedbackApplied> {
        StatsStore::apply_feedback(self, session_id, rating, note, within)
    }
    fn summary(&self) -> Result<Vec<SkillSummary>> {
        StatsStore::summary(self)
    }
    fn recent(&self, skill_name: &str, limit: usize) -> Result<Vec<SkillUsageRow>> {
        StatsStore::recent(self, skill_name, limit)
    }
    fn feedback_count(&self) -> Result<u32> {
        StatsStore::feedback_count(self)
    }
    fn counter_get(&self, key: &str) -> Result<i64> {
        StatsStore::counter_get(self, key)
    }
    fn counter_set(&self, key: &str, value: i64) -> Result<()> {
        StatsStore::counter_set(self, key, value)
    }
    fn counter_incr(&self, key: &str) -> Result<i64> {
        StatsStore::counter_incr(self, key)
    }
}

#[derive(Debug, Clone)]
struct Row {
    id: i64,
    skill_name: String,
    session_id: String,
    ts: chrono::DateTime<Utc>,
    outcome: Outcome,
    signal: Option<String>,
    explicit: bool,
}

/// Skill stats in memory. Same rules, no database.
#[derive(Debug, Default)]
pub struct MemoryStats {
    rows: Mutex<Vec<Row>>,
    feedback: Mutex<usize>,
    counters: Mutex<std::collections::HashMap<String, i64>>,
}

impl MemoryStats {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SkillStats for MemoryStats {
    fn record_invoke(&self, skill_name: &str, session_id: &str) -> Result<i64> {
        let mut rows = self.rows.lock().unwrap();
        let id = rows.len() as i64 + 1;
        rows.push(Row {
            id,
            skill_name: skill_name.to_string(),
            session_id: session_id.to_string(),
            ts: Utc::now(),
            outcome: Outcome::Unclear,
            signal: None,
            explicit: false,
        });
        Ok(id)
    }

    fn set_outcome(&self, id: i64, outcome: Outcome, signal: Option<&str>) -> Result<()> {
        let mut rows = self.rows.lock().unwrap();
        if let Some(row) = rows.iter_mut().find(|r| r.id == id) {
            // The guard that matters: an inferred outcome never replaces one
            // the user stated. The SQL version spells this `AND explicit = 0`.
            if !row.explicit {
                row.outcome = outcome;
                row.signal = signal.map(str::to_string);
            }
        }
        Ok(())
    }

    fn set_outcome_explicit(&self, id: i64, outcome: Outcome, note: Option<&str>) -> Result<()> {
        let mut rows = self.rows.lock().unwrap();
        if let Some(row) = rows.iter_mut().find(|r| r.id == id) {
            row.outcome = outcome;
            row.signal = note.map(str::to_string);
            row.explicit = true;
        }
        Ok(())
    }

    fn apply_feedback(
        &self,
        _session_id: &str,
        rating: Rating,
        note: Option<&str>,
        within: Duration,
    ) -> Result<FeedbackApplied> {
        *self.feedback.lock().unwrap() += 1;
        let cutoff = Utc::now() - within;
        let target = {
            let rows = self.rows.lock().unwrap();
            rows.iter()
                .rfind(|r| !r.explicit && r.ts >= cutoff)
                .map(|r| (r.id, r.skill_name.clone()))
        };
        match target {
            Some((id, skill_name)) => {
                self.set_outcome_explicit(id, rating.outcome(), note)?;
                Ok(FeedbackApplied::ScoredSkill { skill_name })
            }
            None => Ok(FeedbackApplied::RecordedOnly),
        }
    }

    fn summary(&self) -> Result<Vec<SkillSummary>> {
        let rows = self.rows.lock().unwrap();
        let mut names: Vec<String> = rows.iter().map(|r| r.skill_name.clone()).collect();
        names.sort();
        names.dedup();
        Ok(names
            .into_iter()
            .map(|name| {
                let mine = rows.iter().filter(|r| r.skill_name == name);
                let mut s = SkillSummary {
                    skill_name: name.clone(),
                    success: 0,
                    corrected: 0,
                    unclear: 0,
                    total: 0,
                    explicit: 0,
                };
                for r in mine {
                    s.total += 1;
                    if r.explicit {
                        s.explicit += 1;
                    }
                    match r.outcome {
                        Outcome::Success => s.success += 1,
                        Outcome::Corrected => s.corrected += 1,
                        Outcome::Unclear => s.unclear += 1,
                    }
                }
                s
            })
            .collect())
    }

    fn recent(&self, skill_name: &str, limit: usize) -> Result<Vec<SkillUsageRow>> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .iter()
            .filter(|r| r.skill_name == skill_name)
            .rev()
            .take(limit)
            .map(|r| SkillUsageRow {
                id: r.id,
                skill_name: r.skill_name.clone(),
                session_id: r.session_id.clone(),
                ts: r.ts.to_rfc3339(),
                outcome: r.outcome,
                signal: r.signal.clone(),
            })
            .collect())
    }

    fn feedback_count(&self) -> Result<u32> {
        Ok(*self.feedback.lock().unwrap() as u32)
    }

    fn counter_get(&self, key: &str) -> Result<i64> {
        Ok(self.counters.lock().unwrap().get(key).copied().unwrap_or(0))
    }

    fn counter_set(&self, key: &str, value: i64) -> Result<()> {
        self.counters.lock().unwrap().insert(key.to_string(), value);
        Ok(())
    }

    fn counter_incr(&self, key: &str) -> Result<i64> {
        let mut counters = self.counters.lock().unwrap();
        let next = counters.get(key).copied().unwrap_or(0) + 1;
        counters.insert(key.to_string(), next);
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One script, both implementations. These rules are subtle enough that a
    /// version implemented in only one of them is a real risk — this is what
    /// makes having two safe.
    fn behaves_like_skill_stats(stats: &dyn SkillStats) {
        let id = stats.record_invoke("code-reviewer", "sess-1").unwrap();
        assert_eq!(stats.recent("code-reviewer", 5).unwrap().len(), 1);

        // An inferred outcome lands.
        stats
            .set_outcome(id, Outcome::Success, Some("guess"))
            .unwrap();
        assert_eq!(
            stats.recent("code-reviewer", 5).unwrap()[0].outcome,
            Outcome::Success
        );

        // A stated one replaces it…
        stats
            .apply_feedback(
                "sess-1",
                Rating::Bad,
                Some("missed it"),
                Duration::minutes(30),
            )
            .unwrap();
        assert_eq!(
            stats.recent("code-reviewer", 5).unwrap()[0].outcome,
            Outcome::Corrected
        );

        // …and the heuristic must not take it back.
        stats
            .set_outcome(id, Outcome::Success, Some("guess again"))
            .unwrap();
        assert_eq!(
            stats.recent("code-reviewer", 5).unwrap()[0].outcome,
            Outcome::Corrected,
            "an inferred outcome overwrote a stated one"
        );

        // The summary tells the two kinds of evidence apart.
        let summary = stats.summary().unwrap();
        let row = summary
            .iter()
            .find(|r| r.skill_name == "code-reviewer")
            .unwrap();
        assert_eq!(row.total, 1);
        assert_eq!(row.explicit, 1);

        // A rating with nothing recent to attach to is still counted.
        let applied = stats
            .apply_feedback("sess-1", Rating::Good, None, Duration::zero())
            .unwrap();
        assert_eq!(applied, FeedbackApplied::RecordedOnly);
        assert_eq!(stats.feedback_count().unwrap(), 2);

        // Counters: absent reads as zero, and incr returns the new value.
        assert_eq!(stats.counter_get("quiet").unwrap(), 0);
        assert_eq!(stats.counter_incr("quiet").unwrap(), 1);
        assert_eq!(stats.counter_incr("quiet").unwrap(), 2);
        stats.counter_set("quiet", 0).unwrap();
        assert_eq!(stats.counter_get("quiet").unwrap(), 0);
    }

    #[test]
    fn memory_stats_behave_like_skill_stats() {
        behaves_like_skill_stats(&MemoryStats::new());
    }

    #[test]
    fn the_sqlite_store_behaves_like_skill_stats() {
        let path = std::env::temp_dir().join(format!(
            "wingman-skillstats-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = StatsStore::open(&path).unwrap();
        behaves_like_skill_stats(&store);
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// The thing the seam is for: exercising the loop's rules without SQLite.
    #[test]
    fn memory_stats_need_no_database() {
        let stats = MemoryStats::new();
        stats.record_invoke("a", "s").unwrap();
        stats.record_invoke("b", "s").unwrap();
        // Newest unrated wins, which is what `/feedback` relies on.
        match stats
            .apply_feedback("s", Rating::Good, None, Duration::minutes(30))
            .unwrap()
        {
            FeedbackApplied::ScoredSkill { skill_name } => assert_eq!(skill_name, "b"),
            other => panic!("expected the newest invocation, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod hook_without_a_database_tests {
    use super::*;
    use crate::hooks::{LearnConfig, LearnHook};
    use crate::memory::MemoryStore;
    use std::sync::Arc;
    use wingman_core::LearningHook;

    /// What the seam is for: the learning loop's own behaviour, exercised
    /// without SQLite.
    ///
    /// The quiet-session nudge is a rule about counters, not about storage.
    /// Before this it could only be tested by opening a database — and
    /// `StatsStore::open_default()` opens the *user's real* `~/.wingman/
    /// learn.db`, which a test must never touch.
    #[tokio::test]
    async fn the_quiet_session_nudge_reads_its_counter_from_any_backend() {
        let dir = std::env::temp_dir().join(format!(
            "wingman-hook-seam-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let stats: Arc<dyn SkillStats> = Arc::new(MemoryStats::new());
        // Enough quiet sessions that the hook should want to nudge.
        stats.counter_set("sessions_without_save", 99).unwrap();

        let hook = LearnHook::new(
            LearnConfig::new(dir.clone(), "sess-1".into()),
            Arc::new(MemoryStore::new(dir.clone())),
            stats.clone(),
        );

        // The hook reads the counter through the seam; the point is that this
        // runs at all with no database behind it.
        let injected = hook.before_turn(&[]).await;
        assert!(
            injected.is_some(),
            "a long-quiet session should have produced some injected context"
        );
        assert_eq!(stats.counter_get("sessions_without_save").unwrap(), 99);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
