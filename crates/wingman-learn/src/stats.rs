//! Skill usage tracking + outcome scoring.
//!
//! A single global SQLite db at `~/.wingman/learn.db` records every
//! `invoke_skill` call and what happened after it. The agent's
//! [`crate::hooks::LearnHook`] watches subsequent turns and updates the
//! `outcome` field from `unclear` → `success` or `corrected` based on
//! simple negation heuristics.
//!
//! The schema is small on purpose — this isn't analytics, it's just enough
//! signal to know which skills are repeatedly underperforming.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Unclear,
    Success,
    Corrected,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unclear => "unclear",
            Self::Success => "success",
            Self::Corrected => "corrected",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "success" => Self::Success,
            "corrected" => Self::Corrected,
            _ => Self::Unclear,
        }
    }
}

/// What [`StatsStore::apply_feedback`] managed to attach a rating to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackApplied {
    /// The rating scored a recent skill invocation, replacing whatever the
    /// heuristic had guessed for it.
    ScoredSkill { skill_name: String },
    /// Recorded on its own — no recent skill invocation to attach it to.
    RecordedOnly,
}

/// What the user said about a turn, as opposed to what we guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rating {
    Good,
    Bad,
}

impl Rating {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Good => "good",
            Self::Bad => "bad",
        }
    }

    /// Parse a user-typed rating. Accepts the obvious synonyms because this
    /// arrives from someone typing quickly in a TUI, not from a config file.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "good" | "up" | "yes" | "y" | "+" | "👍" => Some(Self::Good),
            "bad" | "down" | "no" | "n" | "-" | "👎" => Some(Self::Bad),
            _ => None,
        }
    }

    /// The skill outcome this rating implies.
    pub fn outcome(self) -> Outcome {
        match self {
            Self::Good => Outcome::Success,
            Self::Bad => Outcome::Corrected,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillUsageRow {
    pub id: i64,
    pub skill_name: String,
    pub session_id: String,
    pub ts: String,
    pub outcome: Outcome,
    pub signal: Option<String>,
}

pub struct StatsStore {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl StatsStore {
    /// Open or create `~/.wingman/learn.db`.
    pub fn open_default() -> Result<Self> {
        let dir = wingman_config::ensure_global_dir()?;
        let path = dir.join("learn.db");
        Self::open(&path)
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS skill_usage (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                skill_name  TEXT NOT NULL,
                session_id  TEXT NOT NULL,
                ts          TEXT NOT NULL,
                outcome     TEXT NOT NULL,
                signal      TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_usage_skill ON skill_usage(skill_name);
             CREATE INDEX IF NOT EXISTS idx_usage_ts ON skill_usage(ts);

             CREATE TABLE IF NOT EXISTS counters (
                key   TEXT PRIMARY KEY,
                value INTEGER NOT NULL
             );

             CREATE TABLE IF NOT EXISTS routing_outcome (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                task_class  TEXT NOT NULL,
                model       TEXT NOT NULL,
                repo        TEXT NOT NULL,
                ts          TEXT NOT NULL,
                passed      INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_routing_repo ON routing_outcome(repo);

             CREATE TABLE IF NOT EXISTS feedback (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id  TEXT NOT NULL,
                ts          TEXT NOT NULL,
                rating      TEXT NOT NULL,
                note        TEXT,
                skill_row   INTEGER
             );
             CREATE INDEX IF NOT EXISTS idx_feedback_ts ON feedback(ts);",
        )?;
        // Added after the table shipped, so existing databases need it
        // grafted on. SQLite has no `ADD COLUMN IF NOT EXISTS`, and the only
        // way to ask is to try — a duplicate-column error means a previous
        // run already did it, which is success, not failure.
        if let Err(e) = conn.execute(
            "ALTER TABLE skill_usage ADD COLUMN explicit INTEGER NOT NULL DEFAULT 0",
            [],
        ) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") {
                return Err(e.into());
            }
        }
        Ok(Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record a fresh skill invocation. Returns the row id so the hook can
    /// flip its outcome later.
    pub fn record_invoke(&self, skill_name: &str, session_id: &str) -> Result<i64> {
        let ts = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO skill_usage(skill_name, session_id, ts, outcome, signal) \
             VALUES (?1, ?2, ?3, 'unclear', NULL)",
            params![skill_name, session_id, ts],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Set an outcome the heuristic inferred.
    ///
    /// Never overwrites one the user stated (`explicit = 1`). Without that
    /// guard, rating a turn and then simply carrying on would silently
    /// replace the rating: the deferred scorer fires on the next user message
    /// and would score the same row from whatever was typed next.
    pub fn set_outcome(&self, id: i64, outcome: Outcome, signal: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE skill_usage SET outcome = ?1, signal = ?2 \
             WHERE id = ?3 AND explicit = 0",
            params![outcome.as_str(), signal, id],
        )?;
        Ok(())
    }

    /// Set an outcome the user stated, rather than one inferred from what
    /// they happened to type next.
    ///
    /// Marked `explicit` so the two can be told apart. They are not the same
    /// evidence: the heuristic scores *any* reply that does not look like a
    /// correction as success, so "thanks" and an unrelated follow-up question
    /// both count. Averaging that together with a real thumbs-down would
    /// launder the one signal that is actually worth something.
    pub fn set_outcome_explicit(
        &self,
        id: i64,
        outcome: Outcome,
        note: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE skill_usage SET outcome = ?1, signal = ?2, explicit = 1 WHERE id = ?3",
            params![outcome.as_str(), note, id],
        )?;
        Ok(())
    }

    /// Record one piece of user feedback.
    ///
    /// Kept even when it resolves no skill row: "that answer was wrong" is
    /// worth having whether or not a skill happened to be involved, and a
    /// rating with nothing to attach to is still a dated record of what the
    /// user thought.
    pub fn record_feedback(
        &self,
        session_id: &str,
        rating: Rating,
        note: Option<&str>,
        skill_row: Option<i64>,
    ) -> Result<i64> {
        let ts = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO feedback(session_id, ts, rating, note, skill_row) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, ts, rating.as_str(), note, skill_row],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Apply a rating the user stated outright, attaching it to the skill
    /// invocation it plausibly refers to.
    ///
    /// The target is resolved from the database rather than from in-memory
    /// state: `/feedback` is a slash command that never reaches the agent
    /// loop, so the loop's pending-row bookkeeping is not available to it, and
    /// a query works from any surface and survives a restart.
    ///
    /// "Plausibly refers to" is the most recent invocation not already rated,
    /// within `within`. A rating given long after the fact should not silently
    /// land on an unrelated skill, so an old row is left alone and the rating
    /// is recorded on its own.
    ///
    /// Rows already scored by the heuristic are still eligible — that is the
    /// point. What the user says beats what was inferred from what they
    /// happened to type next.
    pub fn apply_feedback(
        &self,
        session_id: &str,
        rating: Rating,
        note: Option<&str>,
        within: chrono::Duration,
    ) -> Result<FeedbackApplied> {
        let cutoff = (Utc::now() - within).to_rfc3339();
        let target: Option<(i64, String)> = {
            let conn = self.conn.lock().unwrap();
            conn.query_row(
                "SELECT id, skill_name FROM skill_usage \
                 WHERE explicit = 0 AND ts >= ?1 ORDER BY ts DESC, id DESC LIMIT 1",
                params![cutoff],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
        };
        let row_id = target.as_ref().map(|(id, _)| *id);
        self.record_feedback(session_id, rating, note, row_id)?;
        match target {
            Some((id, skill_name)) => {
                self.set_outcome_explicit(id, rating.outcome(), note)?;
                Ok(FeedbackApplied::ScoredSkill { skill_name })
            }
            None => Ok(FeedbackApplied::RecordedOnly),
        }
    }

    /// The skill name on one usage row, for naming what a rating just scored.
    pub fn skill_name_of(&self, id: i64) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let name = conn
            .query_row(
                "SELECT skill_name FROM skill_usage WHERE id = ?1",
                params![id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(name)
    }

    /// How many ratings the user has given, total.
    pub fn feedback_count(&self) -> Result<u32> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM feedback", [], |r| r.get(0))?;
        Ok(n as u32)
    }

    /// Manually log a final outcome without first calling `record_invoke`.
    /// Used by `/skill rate <name> good|bad`.
    pub fn record_manual(
        &self,
        skill_name: &str,
        session_id: &str,
        outcome: Outcome,
    ) -> Result<()> {
        let ts = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO skill_usage(skill_name, session_id, ts, outcome, signal) \
             VALUES (?1, ?2, ?3, ?4, 'manual')",
            params![skill_name, session_id, ts, outcome.as_str()],
        )?;
        Ok(())
    }

    /// Most recent `limit` rows for `skill_name`, newest first.
    pub fn recent(&self, skill_name: &str, limit: usize) -> Result<Vec<SkillUsageRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, skill_name, session_id, ts, outcome, signal \
             FROM skill_usage WHERE skill_name = ?1 \
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![skill_name, limit as i64], |r| {
            Ok(SkillUsageRow {
                id: r.get(0)?,
                skill_name: r.get(1)?,
                session_id: r.get(2)?,
                ts: r.get(3)?,
                outcome: Outcome::parse(&r.get::<_, String>(4)?),
                signal: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Aggregate by skill name. Useful for `/skill stats` and rewrite
    /// detection: any skill with `corrected >= 3` and `corrected/(success+
    /// corrected) >= 0.5` is a candidate for a rewrite proposal.
    pub fn summary(&self) -> Result<Vec<SkillSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT skill_name, \
                    SUM(CASE WHEN outcome = 'success'   THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN outcome = 'corrected' THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN outcome = 'unclear'   THEN 1 ELSE 0 END), \
                    COUNT(*), \
                    SUM(CASE WHEN explicit = 1 THEN 1 ELSE 0 END) \
             FROM skill_usage GROUP BY skill_name ORDER BY skill_name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SkillSummary {
                skill_name: r.get(0)?,
                success: r.get::<_, i64>(1)? as u32,
                corrected: r.get::<_, i64>(2)? as u32,
                unclear: r.get::<_, i64>(3)? as u32,
                total: r.get::<_, i64>(4)? as u32,
                explicit: r.get::<_, i64>(5)? as u32,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Record the outcome of a routed model call: which `model` served
    /// `task_class` in `repo`, and whether the turn's verification gate passed.
    /// This is the raw signal behind `wingman router stats` (which model wins
    /// per class in this repo).
    pub fn record_routing(
        &self,
        task_class: &str,
        model: &str,
        repo: &str,
        passed: bool,
    ) -> Result<()> {
        let ts = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO routing_outcome(task_class, model, repo, ts, passed) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![task_class, model, repo, ts, passed as i64],
        )?;
        Ok(())
    }

    /// Aggregate routing outcomes by (task_class, model), optionally scoped to
    /// one repo. Ordered by class then by pass-rate descending so the winner
    /// per class is first.
    pub fn routing_summary(&self, repo: Option<&str>) -> Result<Vec<RoutingStat>> {
        let conn = self.conn.lock().unwrap();
        // `?1 IS NULL` short-circuits the repo filter when no repo is given.
        let mut stmt = conn.prepare(
            "SELECT task_class, model, \
                    SUM(passed) AS passes, COUNT(*) AS total \
             FROM routing_outcome \
             WHERE (?1 IS NULL OR repo = ?1) \
             GROUP BY task_class, model \
             ORDER BY task_class ASC, (CAST(SUM(passed) AS REAL) / COUNT(*)) DESC",
        )?;
        let rows = stmt.query_map(params![repo], |r| {
            let passes: i64 = r.get(2)?;
            let total: i64 = r.get(3)?;
            Ok(RoutingStat {
                task_class: r.get(0)?,
                model: r.get(1)?,
                passed: passes as u32,
                total: total as u32,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Count rows newer than `cutoff_iso` for a skill.
    pub fn count_since(&self, skill_name: &str, cutoff_iso: &str) -> Result<u32> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM skill_usage WHERE skill_name = ?1 AND ts >= ?2",
                params![skill_name, cutoff_iso],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(n as u32)
    }

    pub fn counter_get(&self, key: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let v: Option<i64> = conn
            .query_row(
                "SELECT value FROM counters WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.unwrap_or(0))
    }

    pub fn counter_set(&self, key: &str, value: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO counters(key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn counter_incr(&self, key: &str) -> Result<i64> {
        let v = self.counter_get(key)?;
        let next = v + 1;
        self.counter_set(key, next)?;
        Ok(next)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillSummary {
    pub skill_name: String,
    pub success: u32,
    pub corrected: u32,
    pub unclear: u32,
    pub total: u32,
    /// How many of these outcomes the user stated outright, rather than the
    /// phrase heuristic inferring them.
    ///
    /// Reported separately because the two are not comparable evidence: the
    /// heuristic scores any reply that does not look like a correction as
    /// success, so a high success rate with `explicit == 0` means very little.
    pub explicit: u32,
}

/// Aggregated routing outcomes for one (task_class, model) pair.
#[derive(Debug, Clone, Serialize)]
pub struct RoutingStat {
    pub task_class: String,
    pub model: String,
    /// Turns whose verification gate passed.
    pub passed: u32,
    pub total: u32,
}

impl RoutingStat {
    /// Fraction of turns that passed the gate (0.0 when no data).
    pub fn pass_rate(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.passed as f32 / self.total as f32
        }
    }
}

impl SkillSummary {
    /// Fraction of *resolved* (non-unclear) outcomes that were corrections.
    pub fn correction_rate(&self) -> f32 {
        let resolved = self.success + self.corrected;
        if resolved == 0 {
            return 0.0;
        }
        self.corrected as f32 / resolved as f32
    }

    pub fn needs_rewrite(&self) -> bool {
        self.corrected >= 3 && self.correction_rate() >= 0.5
    }
}

/// Heuristic: does `text` look like a correction (the user pushing back on
/// the prior turn)? Catches things like "no", "wait", "don't", "that's
/// wrong", "actually", and similar at the start of the message.
pub fn looks_like_correction(text: &str) -> Option<&'static str> {
    let lower = text.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    const SIGNALS: &[&str] = &[
        "no,",
        "no ",
        "no.",
        "wait,",
        "wait ",
        "don't",
        "dont ",
        "do not",
        "that's wrong",
        "thats wrong",
        "that is wrong",
        "wrong,",
        "wrong.",
        "actually,",
        "actually ",
        "nope",
        "stop",
        "incorrect",
        "not what i",
    ];
    SIGNALS.iter().copied().find(|s| lower.starts_with(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db() -> PathBuf {
        std::env::temp_dir().join(format!(
            "wingman-learn-stats-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// The whole point: what the user says beats what was inferred from what
    /// they happened to type next.
    #[test]
    fn an_explicit_rating_survives_the_heuristic() {
        let p = tmp_db();
        let store = StatsStore::open(&p).unwrap();
        let id = store.record_invoke("code-reviewer", "sess-1").unwrap();

        store
            .apply_feedback(
                "sess-1",
                Rating::Bad,
                Some("missed the bug"),
                chrono::Duration::minutes(30),
            )
            .unwrap();
        assert_eq!(
            store.recent("code-reviewer", 5).unwrap()[0].outcome,
            Outcome::Corrected
        );

        // The deferred scorer fires on the next user message and would
        // otherwise score this same row from whatever was typed next.
        store
            .set_outcome(id, Outcome::Success, Some("heuristic"))
            .unwrap();
        let rows = store.recent("code-reviewer", 5).unwrap();
        assert_eq!(
            rows[0].outcome,
            Outcome::Corrected,
            "the heuristic overwrote what the user stated"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn feedback_scores_the_most_recent_unrated_invocation() {
        let p = tmp_db();
        let store = StatsStore::open(&p).unwrap();
        store.record_invoke("older", "sess-1").unwrap();
        store.record_invoke("newest", "sess-1").unwrap();

        let applied = store
            .apply_feedback("sess-1", Rating::Good, None, chrono::Duration::minutes(30))
            .unwrap();
        match applied {
            FeedbackApplied::ScoredSkill { skill_name } => assert_eq!(skill_name, "newest"),
            other => panic!("expected the newest invocation, got {other:?}"),
        }
        // The older one is untouched - a rating refers to what you just saw.
        assert_eq!(
            store.recent("older", 5).unwrap()[0].outcome,
            Outcome::Unclear
        );
        let _ = std::fs::remove_file(&p);
    }

    /// A rating given long after the fact must not land on an unrelated skill.
    #[test]
    fn stale_invocations_are_out_of_reach() {
        let p = tmp_db();
        let store = StatsStore::open(&p).unwrap();
        store.record_invoke("yesterdays-work", "sess-1").unwrap();

        let applied = store
            .apply_feedback("sess-1", Rating::Bad, None, chrono::Duration::zero())
            .unwrap();
        assert_eq!(applied, FeedbackApplied::RecordedOnly);
        assert_eq!(
            store.recent("yesterdays-work", 5).unwrap()[0].outcome,
            Outcome::Unclear
        );
        // Still recorded, though - the opinion is worth keeping either way.
        assert_eq!(store.feedback_count().unwrap(), 1);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_second_rating_does_not_re_score_the_same_row() {
        let p = tmp_db();
        let store = StatsStore::open(&p).unwrap();
        store.record_invoke("skill-a", "sess-1").unwrap();
        let first = store
            .apply_feedback("sess-1", Rating::Good, None, chrono::Duration::minutes(30))
            .unwrap();
        assert!(matches!(first, FeedbackApplied::ScoredSkill { .. }));
        // Already rated, so it is no longer a candidate.
        let second = store
            .apply_feedback("sess-1", Rating::Bad, None, chrono::Duration::minutes(30))
            .unwrap();
        assert_eq!(second, FeedbackApplied::RecordedOnly);
        assert_eq!(
            store.recent("skill-a", 5).unwrap()[0].outcome,
            Outcome::Success
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn summary_separates_stated_outcomes_from_inferred_ones() {
        let p = tmp_db();
        let store = StatsStore::open(&p).unwrap();
        let inferred = store.record_invoke("skill-a", "sess-1").unwrap();
        store
            .set_outcome(inferred, Outcome::Success, Some("guess"))
            .unwrap();
        store.record_invoke("skill-a", "sess-1").unwrap();
        store
            .apply_feedback("sess-1", Rating::Good, None, chrono::Duration::minutes(30))
            .unwrap();

        let sum = store.summary().unwrap();
        let row = sum.iter().find(|r| r.skill_name == "skill-a").unwrap();
        assert_eq!(row.success, 2);
        assert_eq!(row.explicit, 1, "only one of these was stated by the user");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn ratings_parse_the_words_people_actually_type() {
        for good in ["good", "up", "yes", "y", "+", "GOOD"] {
            assert_eq!(Rating::parse(good), Some(Rating::Good), "{good}");
        }
        for bad in ["bad", "down", "no", "n", "-"] {
            assert_eq!(Rating::parse(bad), Some(Rating::Bad), "{bad}");
        }
        assert_eq!(Rating::parse("maybe"), None);
        assert_eq!(Rating::parse(""), None);
    }

    #[test]
    fn record_invoke_then_set_outcome() {
        let p = tmp_db();
        let store = StatsStore::open(&p).unwrap();
        let id = store.record_invoke("code-reviewer", "sess-1").unwrap();
        store.set_outcome(id, Outcome::Success, None).unwrap();
        let rows = store.recent("code-reviewer", 5).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, Outcome::Success);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn summary_counts_by_outcome() {
        let p = tmp_db();
        let store = StatsStore::open(&p).unwrap();
        let a = store.record_invoke("foo", "s").unwrap();
        store.set_outcome(a, Outcome::Success, None).unwrap();
        let b = store.record_invoke("foo", "s").unwrap();
        store
            .set_outcome(b, Outcome::Corrected, Some("no,"))
            .unwrap();
        let c = store.record_invoke("foo", "s").unwrap();
        store
            .set_outcome(c, Outcome::Corrected, Some("wrong,"))
            .unwrap();
        let d = store.record_invoke("foo", "s").unwrap();
        store
            .set_outcome(d, Outcome::Corrected, Some("don't"))
            .unwrap();
        let sum = store.summary().unwrap();
        assert_eq!(sum.len(), 1);
        assert_eq!(sum[0].success, 1);
        assert_eq!(sum[0].corrected, 3);
        assert!(sum[0].needs_rewrite());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn negation_detection() {
        assert!(looks_like_correction("no, that's not it").is_some());
        assert!(looks_like_correction("Don't do that").is_some());
        assert!(looks_like_correction("ok, looks good").is_none());
        assert!(looks_like_correction("Actually, try this").is_some());
    }

    #[test]
    fn counters_round_trip() {
        let p = tmp_db();
        let store = StatsStore::open(&p).unwrap();
        assert_eq!(store.counter_get("sessions_without_save").unwrap(), 0);
        assert_eq!(store.counter_incr("sessions_without_save").unwrap(), 1);
        assert_eq!(store.counter_incr("sessions_without_save").unwrap(), 2);
        store.counter_set("sessions_without_save", 0).unwrap();
        assert_eq!(store.counter_get("sessions_without_save").unwrap(), 0);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn routing_summary_ranks_winner_first() {
        let p = tmp_db();
        let store = StatsStore::open(&p).unwrap();
        // opus: 2/2 pass; haiku: 1/3 pass — both on "default" in repo "r".
        store.record_routing("default", "opus", "r", true).unwrap();
        store.record_routing("default", "opus", "r", true).unwrap();
        store.record_routing("default", "haiku", "r", true).unwrap();
        store
            .record_routing("default", "haiku", "r", false)
            .unwrap();
        store
            .record_routing("default", "haiku", "r", false)
            .unwrap();
        // Different repo — excluded when scoped to "r".
        store
            .record_routing("default", "haiku", "other", true)
            .unwrap();

        let stats = store.routing_summary(Some("r")).unwrap();
        assert_eq!(stats.len(), 2);
        // Winner (higher pass-rate) first within the class.
        assert_eq!(stats[0].model, "opus");
        assert_eq!(stats[0].total, 2);
        assert!((stats[0].pass_rate() - 1.0).abs() < 1e-6);
        assert_eq!(stats[1].model, "haiku");
        assert_eq!(stats[1].total, 3); // "other" repo excluded

        // Unscoped includes the other repo.
        let all = store.routing_summary(None).unwrap();
        let haiku_total: u32 = all
            .iter()
            .filter(|s| s.model == "haiku")
            .map(|s| s.total)
            .sum();
        assert_eq!(haiku_total, 4);
        let _ = std::fs::remove_file(&p);
    }
}
