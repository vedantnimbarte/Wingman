//! Run roll-ups: the ephemeral half of the board.
//!
//! Everything here is *derived* from a run's `state.json`. The board never
//! writes run state; it reads through `wingman_autonomous::dashboard` and
//! caches the result keyed by the snapshot's mtime.
//!
//! Ten projects times forty runs is 400 file reads per frame if done naively.
//! The cache makes a terminal run cost one `stat`. A resumed run rewrites
//! `state.json`, which changes the mtime, which invalidates the entry — so
//! `pilot resume` needs no special case.

use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use wingman_autonomous::{dashboard, RunState, RunStatus, TaskStatus};

use crate::store::{BoardStore, Result};

/// One planner task, flattened for display. Mirrors `dashboard::TaskRow` plus
/// the agent fields a card needs (name, model, transcript).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubRow {
    pub task_id: String,
    pub title: String,
    pub status: TaskStatus,
    pub role: String,
    pub agent_name: Option<String>,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub usd: f64,
    pub attempts: u32,
    /// Unmet dependencies — why the scheduler is holding this task.
    pub blocked_by: Vec<String>,
    pub current_tool: Option<String>,
    /// Every declared dependency, met or not. `blocked_by` is the subset the
    /// scheduler is still waiting on.
    #[serde(default)]
    pub deps: Vec<String>,
    /// How many paths the task declared it would write.
    #[serde(default)]
    pub writes: usize,
    /// Wall time from first `in_progress` to now (or to the terminal status).
    #[serde(default)]
    pub elapsed_secs: Option<i64>,
    /// Worker's own summary, once it reported one.
    #[serde(default)]
    pub outcome: Option<String>,
    /// Worktree the task was assigned, relative to the repo root.
    #[serde(default)]
    pub worktree: Option<String>,
}

/// A run, summarised for one card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rollup {
    pub status: RunStatus,
    pub done: usize,
    pub total: usize,
    pub failed: usize,
    pub blocked: usize,
    pub review: usize,
    /// Tasks a worker is actively holding.
    #[serde(default)]
    pub in_progress: usize,
    /// Tasks that have not started yet (`pending` or `todo`).
    #[serde(default)]
    pub not_started: usize,
    pub usd: f64,
    pub subrows: Vec<SubRow>,
}

impl Rollup {
    /// Derive a roll-up from a run snapshot.
    pub fn from_state(state: &RunState) -> Self {
        let done_ids: std::collections::HashSet<&str> = state
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Done)
            .map(|t| t.id.as_str())
            .collect();

        let mut r = Rollup {
            status: state.status,
            done: 0,
            total: state.tasks.len(),
            failed: 0,
            blocked: 0,
            review: 0,
            in_progress: 0,
            not_started: 0,
            usd: state.totals.usd,
            subrows: Vec::with_capacity(state.tasks.len()),
        };

        for t in &state.tasks {
            match t.status {
                TaskStatus::Done => r.done += 1,
                TaskStatus::Failed => r.failed += 1,
                TaskStatus::Blocked => r.blocked += 1,
                TaskStatus::Review => r.review += 1,
                TaskStatus::InProgress => r.in_progress += 1,
                TaskStatus::Pending | TaskStatus::Todo => r.not_started += 1,
            }
            let agent = t.agent.as_deref().and_then(|id| state.agent(id));
            r.subrows.push(SubRow {
                task_id: t.id.clone(),
                title: t.title.clone(),
                status: t.status,
                role: t.role.as_str().to_string(),
                agent_name: agent.map(|a| {
                    if a.name.is_empty() {
                        a.id.clone()
                    } else {
                        a.name.clone()
                    }
                }),
                model: agent.and_then(|a| a.model.clone()),
                session_id: agent.and_then(|a| a.session_id.clone()),
                usd: t.usd,
                attempts: t.attempts,
                blocked_by: blocked_by(t, &done_ids),
                current_tool: agent.and_then(|a| a.current_tool.clone()),
                deps: t.deps.clone(),
                writes: t.writes.len(),
                elapsed_secs: elapsed_secs(t),
                outcome: t.outcome.as_ref().map(|o| o.summary.clone()),
                worktree: t.worktree.clone(),
            });
        }
        r
    }

    /// Whether the run is over, however it ended.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            RunStatus::Done | RunStatus::Failed | RunStatus::Aborted
        )
    }

    /// Any task that has been retried at least once.
    pub fn retried(&self) -> bool {
        self.subrows.iter().any(|s| s.attempts > 1)
    }
}

/// Dependencies not yet `Done`, for a task that has not started. Reconstructed
/// from `RunState` alone — no coupling to the scheduler.
fn blocked_by(
    task: &wingman_autonomous::Task,
    done: &std::collections::HashSet<&str>,
) -> Vec<String> {
    if !matches!(task.status, TaskStatus::Pending | TaskStatus::Todo) {
        return Vec::new();
    }
    task.deps
        .iter()
        .filter(|d| !done.contains(d.as_str()))
        .cloned()
        .collect()
}

/// Wall time the task has been running, or ran for. `None` until it starts.
fn elapsed_secs(task: &wingman_autonomous::Task) -> Option<i64> {
    let start = chrono::DateTime::parse_from_rfc3339(task.started_at.as_deref()?).ok()?;
    let end = match task.ended_at.as_deref() {
        Some(e) => chrono::DateTime::parse_from_rfc3339(e).ok()?.to_utc(),
        None => chrono::Utc::now(),
    };
    Some((end - start.to_utc()).num_seconds().max(0))
}

/// `state.json` mtime in nanoseconds, or `None` when the run is gone.
fn mtime_ns(run_dir: &Path) -> Option<i64> {
    let t = dashboard::state_mtime(run_dir)?;
    let d = t.duration_since(UNIX_EPOCH).ok()?;
    Some(d.as_nanos() as i64)
}

impl BoardStore {
    /// Read a run's roll-up, using the cache when `state.json` has not moved.
    ///
    /// Returns `None` when the run directory is gone — the caller closes the
    /// dispatch out and the card falls back to Backlog.
    pub fn rollup_for(&self, run_dir: &Path) -> Result<Option<Rollup>> {
        let Some(mtime) = mtime_ns(run_dir) else {
            return Ok(None);
        };
        let key = run_dir.to_string_lossy().to_string();

        if let Some(hit) = self.cached_rollup(&key, mtime)? {
            return Ok(Some(hit));
        }

        let state = match dashboard::load_state(run_dir) {
            Ok(s) => s,
            Err(e) => {
                // A corrupt snapshot must not wedge the whole board; this one
                // card renders as `missing` and the rest is unaffected.
                tracing::debug!(target: "board::rollup", "unreadable run {key}: {e}");
                return Ok(None);
            }
        };
        let rollup = Rollup::from_state(&state);
        self.cache_rollup(&key, mtime, &rollup)?;
        Ok(Some(rollup))
    }

    fn cached_rollup(&self, key: &str, mtime: i64) -> Result<Option<Rollup>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT subrows, mtime_ns FROM rollup WHERE run_dir = ?1")?;
        let mut rows = stmt.query([key])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        if row.get::<_, i64>(1)? != mtime {
            return Ok(None);
        }
        let json: String = row.get(0)?;
        Ok(serde_json::from_str(&json).ok())
    }

    fn cache_rollup(&self, key: &str, mtime: i64, r: &Rollup) -> Result<()> {
        // The whole roll-up is stored as JSON; the scalar columns exist so the
        // cache stays inspectable with plain SQL when something looks wrong.
        let json = serde_json::to_string(r).unwrap_or_default();
        self.lock().execute(
            "INSERT INTO rollup (run_dir, mtime_ns, status, done, total, failed, blocked, review, usd, subrows)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(run_dir) DO UPDATE SET
                mtime_ns = excluded.mtime_ns, status = excluded.status,
                done = excluded.done, total = excluded.total,
                failed = excluded.failed, blocked = excluded.blocked,
                review = excluded.review, usd = excluded.usd,
                subrows = excluded.subrows",
            (
                key,
                mtime,
                format!("{:?}", r.status).to_lowercase(),
                r.done as i64,
                r.total as i64,
                r.failed as i64,
                r.blocked as i64,
                r.review as i64,
                r.usd,
                json,
            ),
        )?;
        Ok(())
    }

    /// Drop the whole cache. Always safe — it is derived data.
    pub fn clear_rollup_cache(&self) -> Result<()> {
        self.lock().execute("DELETE FROM rollup", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::store;
    use wingman_autonomous::{Role, Task};

    fn state_with(tasks: Vec<Task>, status: RunStatus) -> RunState {
        let mut s = RunState::new("r1", "goal", "deadbeef", "wingman/auto/r1");
        s.status = status;
        s.tasks = tasks;
        s
    }

    fn task(id: &str, status: TaskStatus, deps: &[&str]) -> Task {
        let mut t = Task::new(id, Role::Developer, format!("task {id}"));
        t.status = status;
        t.deps = deps.iter().map(|s| s.to_string()).collect();
        t
    }

    #[test]
    fn counts_every_status() {
        let s = state_with(
            vec![
                task("t1", TaskStatus::Done, &[]),
                task("t2", TaskStatus::Failed, &[]),
                task("t3", TaskStatus::Blocked, &[]),
                task("t4", TaskStatus::Review, &[]),
                task("t5", TaskStatus::InProgress, &[]),
            ],
            RunStatus::Running,
        );
        let r = Rollup::from_state(&s);
        assert_eq!(
            (r.done, r.failed, r.blocked, r.review, r.total),
            (1, 1, 1, 1, 5)
        );
    }

    #[test]
    fn blocked_by_lists_only_unmet_deps() {
        let s = state_with(
            vec![
                task("t1", TaskStatus::Done, &[]),
                task("t2", TaskStatus::InProgress, &[]),
                task("t3", TaskStatus::Pending, &["t1", "t2"]),
            ],
            RunStatus::Running,
        );
        let r = Rollup::from_state(&s);
        let t3 = r.subrows.iter().find(|s| s.task_id == "t3").unwrap();
        assert_eq!(t3.blocked_by, vec!["t2"], "t1 is done, t2 is not");
    }

    #[test]
    fn started_tasks_are_never_blocked_by() {
        let s = state_with(
            vec![
                task("t1", TaskStatus::InProgress, &[]),
                task("t2", TaskStatus::InProgress, &["t1"]),
            ],
            RunStatus::Running,
        );
        let r = Rollup::from_state(&s);
        let t2 = r.subrows.iter().find(|s| s.task_id == "t2").unwrap();
        assert!(t2.blocked_by.is_empty());
    }

    #[test]
    fn subrow_carries_agent_model_and_session() {
        let mut s = state_with(
            vec![task("t1", TaskStatus::InProgress, &[])],
            RunStatus::Running,
        );
        s.tasks[0].agent = Some("a1".into());
        s.agents.push(wingman_autonomous::Agent {
            id: "a1".into(),
            name: "brave_otter".into(),
            role: Role::Developer,
            current_task: Some("t1".into()),
            pid: Some(1),
            status: wingman_autonomous::AgentStatus::InProgress,
            session_id: Some("sess-1".into()),
            spawned_at: None,
            current_tool: Some("edit_file".into()),
            usd: 0.4,
            model: Some("opus-5".into()),
        });

        let r = Rollup::from_state(&s);
        let row = &r.subrows[0];
        assert_eq!(row.agent_name.as_deref(), Some("brave_otter"));
        assert_eq!(row.model.as_deref(), Some("opus-5"));
        assert_eq!(row.session_id.as_deref(), Some("sess-1"));
        assert_eq!(row.current_tool.as_deref(), Some("edit_file"));
    }

    #[test]
    fn missing_run_dir_yields_none() {
        let (dir, s) = store();
        assert!(s.rollup_for(&dir.path().join("nope")).unwrap().is_none());
    }

    #[test]
    fn cache_hits_until_state_json_changes() {
        let (dir, s) = store();
        let run = dir.path().join("run1");
        std::fs::create_dir_all(&run).unwrap();

        let mut st = state_with(vec![task("t1", TaskStatus::Done, &[])], RunStatus::Done);
        std::fs::write(run.join("state.json"), serde_json::to_string(&st).unwrap()).unwrap();

        let first = s.rollup_for(&run).unwrap().unwrap();
        assert_eq!(first.done, 1);

        // Rewrite the file with different content but keep the cache honest by
        // checking that a *stale* mtime still serves the old value.
        let cached = s.rollup_for(&run).unwrap().unwrap();
        assert_eq!(cached, first, "unchanged mtime must hit the cache");

        // A real change: bump mtime by rewriting with new content.
        st.tasks.push(task("t2", TaskStatus::Done, &[]));
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(run.join("state.json"), serde_json::to_string(&st).unwrap()).unwrap();

        let fresh = s.rollup_for(&run).unwrap().unwrap();
        assert_eq!(fresh.total, 2, "touched state.json must miss the cache");
    }

    #[test]
    fn corrupt_state_is_skipped_not_fatal() {
        let (dir, s) = store();
        let run = dir.path().join("run1");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("state.json"), "{ not json").unwrap();
        assert!(s.rollup_for(&run).unwrap().is_none());
    }

    #[test]
    fn subrow_carries_detail_fields() {
        let mut t = task("t1", TaskStatus::Done, &["t0"]);
        t.writes = vec!["a.rs".into(), "b.rs".into()];
        t.worktree = Some(".wingman/worktrees/auto-r1-t1".into());
        t.started_at = Some("2026-08-21T10:00:00Z".into());
        t.ended_at = Some("2026-08-21T10:02:30Z".into());
        t.outcome = Some(wingman_autonomous::TaskOutcome {
            summary: "debounced the watcher".into(),
            files_changed: vec!["a.rs".into()],
        });

        let r = Rollup::from_state(&state_with(vec![t], RunStatus::Done));
        let s = &r.subrows[0];
        assert_eq!(s.deps, vec!["t0"], "all deps, not just unmet ones");
        assert_eq!(s.writes, 2);
        assert_eq!(s.elapsed_secs, Some(150));
        assert_eq!(s.outcome.as_deref(), Some("debounced the watcher"));
        assert!(s.worktree.is_some());
    }

    #[test]
    fn elapsed_is_none_until_a_task_starts() {
        let r = Rollup::from_state(&state_with(
            vec![task("t1", TaskStatus::Pending, &[])],
            RunStatus::Running,
        ));
        assert_eq!(r.subrows[0].elapsed_secs, None);
    }
}
