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
             CREATE INDEX IF NOT EXISTS idx_routing_repo ON routing_outcome(repo);",
        )?;
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

    pub fn set_outcome(&self, id: i64, outcome: Outcome, signal: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE skill_usage SET outcome = ?1, signal = ?2 WHERE id = ?3",
            params![outcome.as_str(), signal, id],
        )?;
        Ok(())
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
                    COUNT(*) \
             FROM skill_usage GROUP BY skill_name ORDER BY skill_name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SkillSummary {
                skill_name: r.get(0)?,
                success: r.get::<_, i64>(1)? as u32,
                corrected: r.get::<_, i64>(2)? as u32,
                unclear: r.get::<_, i64>(3)? as u32,
                total: r.get::<_, i64>(4)? as u32,
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
