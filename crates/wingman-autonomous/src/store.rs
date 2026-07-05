//! Run-store: append-only [`Event`] log + atomic [`RunState`] snapshot.
//!
//! On disk, every run lives under `<project>/.wingman/autonomous/<run-id>/`:
//!
//! ```text
//! <run-id>/
//!   tasks.jsonl   # append-only event log; one Event per line
//!   state.json    # latest RunState snapshot, rewritten atomically each event
//! ```
//!
//! Only the [`RunStore`] writes to these files. The manager and workers do
//! not write them directly — they call the store via tool / IPC paths so
//! writes stay serialised and the snapshot can never lag behind the log.
//!
//! ## Crash safety
//!
//! The append is durable on its own: `tasks.jsonl` is the source of truth.
//! `state.json` is written through a temp-and-rename so a crash mid-write
//! either leaves the previous snapshot intact or installs the new one
//! atomically. On reopen, [`RunStore::load`] replays the JSONL and
//! refreshes the snapshot — disagreement between the two is resolved in
//! favour of the log.

use std::path::{Path, PathBuf};

use chrono::Utc;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;

use crate::model::{apply, Event, RunState};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde_json: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("malformed event on line {line} of {path}: {source}")]
    BadEvent {
        path: PathBuf,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("run directory {0} has no tasks.jsonl — cannot load")]
    NoLog(PathBuf),
}

/// Capacity of the broadcast channel that fans events out to the TUI.
const EVENT_BROADCAST_CAPACITY: usize = 256;

/// Owner of the per-run on-disk artefacts and the in-memory snapshot.
///
/// `RunStore` is the single writer for both `tasks.jsonl` and `state.json`.
/// Use [`RunStore::create`] for a brand-new run, [`RunStore::load`] to
/// reattach to one already on disk (e.g. `wingman pilot resume`).
pub struct RunStore {
    dir: PathBuf,
    log: tokio::fs::File,
    state: RunState,
    tx: broadcast::Sender<Event>,
}

impl RunStore {
    /// Initialise a fresh run under `dir`. Creates the directory if missing
    /// and seeds the log with a `run.start` event. Returns the store with
    /// its snapshot already updated.
    ///
    /// Use [`RunStore::create_quiet`] from tests / planner-only flows that
    /// have already chosen a base commit and integration branch upstream.
    pub async fn create(
        dir: impl AsRef<Path>,
        run_id: impl Into<String>,
        goal: impl Into<String>,
        base_commit: impl Into<String>,
        integration_branch: impl Into<String>,
    ) -> Result<Self, StoreError> {
        let dir = dir.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&dir).await?;
        let log_path = dir.join("tasks.jsonl");
        let log = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await?;
        let run_id = run_id.into();
        let goal = goal.into();
        let base_commit = base_commit.into();
        let integration_branch = integration_branch.into();
        let state = RunState::new(&run_id, &goal, &base_commit, &integration_branch);
        let (tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let mut store = Self {
            dir,
            log,
            state,
            tx,
        };
        store
            .append(Event::RunStart {
                t: now(),
                run_id,
                goal,
                base_commit,
                integration_branch,
            })
            .await?;
        Ok(store)
    }

    /// Reopen an existing run, replaying its log to reconstruct state.
    ///
    /// If `state.json` exists it is ignored on read — the log wins. We do
    /// rewrite the snapshot at the end of replay so on-disk artefacts agree
    /// again.
    pub async fn load(dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let dir = dir.as_ref().to_path_buf();
        let log_path = dir.join("tasks.jsonl");
        if !log_path.exists() {
            return Err(StoreError::NoLog(dir));
        }
        let body = tokio::fs::read_to_string(&log_path).await?;

        // Seed with empty state; the first replayed event is expected to be
        // RunStart which fills in the identifying fields. Robust against
        // logs missing it (e.g. legacy / corrupted) — caller still gets the
        // tasks back.
        let mut state = RunState::new(String::new(), String::new(), String::new(), String::new());

        for (i, line) in body.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let event: Event =
                serde_json::from_str(line).map_err(|source| StoreError::BadEvent {
                    path: log_path.clone(),
                    line: i + 1,
                    source,
                })?;
            apply(&mut state, &event);
        }

        let log = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await?;
        let (tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let store = Self {
            dir,
            log,
            state,
            tx,
        };
        // Refresh the snapshot so a stale state.json doesn't outlive a
        // crash — the on-disk picture should match what we just replayed.
        store.snapshot().await?;
        Ok(store)
    }

    /// Path of the run directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of the JSONL log.
    pub fn log_path(&self) -> PathBuf {
        self.dir.join("tasks.jsonl")
    }

    /// Path of the state snapshot.
    pub fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    /// Read and parse the full event log from disk. Unlike [`state`],
    /// which only keeps the reconstructed snapshot, this returns every
    /// [`Event`] in order — needed by consumers that reason over the
    /// activity stream (E11 checkpoint hygiene, R2 feedback). Blank lines
    /// are skipped; a malformed line aborts with [`StoreError::BadEvent`].
    pub async fn read_events(&self) -> Result<Vec<Event>, StoreError> {
        let log_path = self.log_path();
        let body = match tokio::fs::read_to_string(&log_path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for (i, line) in body.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let event: Event =
                serde_json::from_str(line).map_err(|source| StoreError::BadEvent {
                    path: log_path.clone(),
                    line: i + 1,
                    source,
                })?;
            out.push(event);
        }
        Ok(out)
    }

    /// Borrow the current in-memory snapshot.
    pub fn state(&self) -> &RunState {
        &self.state
    }

    /// Subscribe to live events. Subscribers see every event appended *after*
    /// they subscribe; replay the log for backfill if needed.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Append an event: write to `tasks.jsonl`, apply to the snapshot,
    /// rewrite `state.json`, broadcast to subscribers.
    ///
    /// Returns once the JSONL line is flushed to disk. The snapshot rewrite
    /// is best-effort — a failure to write `state.json` is logged but not
    /// surfaced, because the log is the source of truth and the snapshot can
    /// be rebuilt by `load`.
    pub async fn append(&mut self, event: Event) -> Result<(), StoreError> {
        let line = serde_json::to_string(&event)?;
        self.log.write_all(line.as_bytes()).await?;
        self.log.write_all(b"\n").await?;
        self.log.flush().await?;

        apply(&mut self.state, &event);

        if let Err(e) = self.snapshot().await {
            tracing::warn!(error = %e, "failed to write state.json snapshot");
        }

        // Ignore send errors: it just means there are no live subscribers.
        let _ = self.tx.send(event);
        Ok(())
    }

    /// Convenience for callers that want a timestamp without going through
    /// `chrono` themselves.
    pub fn now() -> String {
        now()
    }

    /// Write `state.json` atomically: serialise to a sibling tempfile then
    /// rename over the target. Either the previous snapshot survives or the
    /// new one is installed — never a half-written file.
    async fn snapshot(&self) -> Result<(), StoreError> {
        let path = self.state_path();
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(&self.state)?;
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Role, RunStatus, TaskStatus};
    use tempfile::tempdir;

    #[tokio::test]
    async fn create_appends_run_start() {
        let dir = tempdir().unwrap();
        let store = RunStore::create(dir.path(), "r1", "goal", "deadbeef", "wingman/auto/r1")
            .await
            .unwrap();
        assert_eq!(store.state().run_id, "r1");
        assert_eq!(store.state().goal, "goal");
        assert_eq!(store.state().base_commit, "deadbeef");
        assert_eq!(store.state().integration_branch, "wingman/auto/r1");
        assert!(store.log_path().exists());
        assert!(store.state_path().exists());
    }

    #[tokio::test]
    async fn append_then_reload_yields_same_state() {
        let dir = tempdir().unwrap();

        let mut store = RunStore::create(dir.path(), "r1", "goal", "deadbeef", "wingman/auto/r1")
            .await
            .unwrap();

        store
            .append(Event::TaskCreate {
                t: RunStore::now(),
                id: "t1".into(),
                role: Role::Developer,
                title: "Add --version-only flag".into(),
                goal: "wire a fast-exit flag".into(),
                deps: vec![],
                writes: vec!["crates/wingman-cli/src/main.rs".into()],
                acceptance: vec![],
                reversibility: Default::default(),
                reversibility_reason: None,
            })
            .await
            .unwrap();

        store
            .append(Event::TaskAssign {
                t: RunStore::now(),
                id: "t1".into(),
                agent: "agent-1".into(),
                worktree: ".wingman/worktrees/auto-r1-t1".into(),
            })
            .await
            .unwrap();

        store
            .append(Event::AgentSpawn {
                t: RunStore::now(),
                agent: "agent-1".into(),
                role: Role::Developer,
                pid: Some(12345),
                session_id: Some("sess-1".into()),
            })
            .await
            .unwrap();

        store
            .append(Event::TaskStatus {
                t: RunStore::now(),
                id: "t1".into(),
                status: TaskStatus::InProgress,
                outcome: None,
            })
            .await
            .unwrap();

        store
            .append(Event::AgentUsd {
                t: RunStore::now(),
                agent: "agent-1".into(),
                model: "claude-haiku-4-5".into(),
                input_tokens: 1000,
                output_tokens: 500,
                usd: 0.07,
            })
            .await
            .unwrap();

        let original = store.state().clone();
        drop(store); // Simulate process exit.

        let reloaded = RunStore::load(dir.path()).await.unwrap();
        let s = reloaded.state();

        assert_eq!(s.run_id, original.run_id);
        assert_eq!(s.goal, original.goal);
        assert_eq!(s.base_commit, original.base_commit);
        assert_eq!(s.integration_branch, original.integration_branch);
        assert_eq!(s.tasks.len(), 1);
        let t = &s.tasks[0];
        assert_eq!(t.id, "t1");
        assert_eq!(t.status, TaskStatus::InProgress);
        assert_eq!(t.agent.as_deref(), Some("agent-1"));
        assert_eq!(t.worktree.as_deref(), Some(".wingman/worktrees/auto-r1-t1"));
        assert!((t.usd - 0.07).abs() < 1e-9);
        assert_eq!(s.agents.len(), 1);
        assert_eq!(s.agents[0].id, "agent-1");
        assert_eq!(s.agents[0].current_task.as_deref(), Some("t1"));
        assert_eq!(s.agents[0].pid, Some(12345));
        assert_eq!(s.agents[0].session_id.as_deref(), Some("sess-1"));
        assert!((s.totals.usd - 0.07).abs() < 1e-9);
        assert_eq!(s.totals.tokens_in, 1000);
        assert_eq!(s.totals.tokens_out, 500);
    }

    #[tokio::test]
    async fn run_done_event_marks_status() {
        let dir = tempdir().unwrap();
        let mut store = RunStore::create(dir.path(), "r1", "goal", "deadbeef", "wingman/auto/r1")
            .await
            .unwrap();
        store
            .append(Event::RunDone { t: RunStore::now() })
            .await
            .unwrap();
        assert_eq!(store.state().status, RunStatus::Done);
    }

    #[tokio::test]
    async fn subscribe_receives_events() {
        let dir = tempdir().unwrap();
        let mut store = RunStore::create(dir.path(), "r1", "goal", "deadbeef", "wingman/auto/r1")
            .await
            .unwrap();
        let mut rx = store.subscribe();
        store
            .append(Event::TaskCreate {
                t: RunStore::now(),
                id: "t1".into(),
                role: Role::Developer,
                title: "do thing".into(),
                goal: "".into(),
                deps: vec![],
                writes: vec![],
                acceptance: vec![],
                reversibility: Default::default(),
                reversibility_reason: None,
            })
            .await
            .unwrap();
        let ev = rx.recv().await.unwrap();
        matches!(ev, Event::TaskCreate { .. });
    }

    #[tokio::test]
    async fn load_rejects_missing_log() {
        let dir = tempdir().unwrap();
        match RunStore::load(dir.path()).await {
            Err(StoreError::NoLog(_)) => {}
            Err(other) => panic!("expected NoLog, got {other:?}"),
            Ok(_) => panic!("expected NoLog error, got Ok"),
        }
    }

    #[tokio::test]
    async fn load_rejects_malformed_line() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("tasks.jsonl");
        tokio::fs::write(&log, b"this is not json\n").await.unwrap();
        match RunStore::load(dir.path()).await {
            Err(StoreError::BadEvent { .. }) => {}
            Err(other) => panic!("expected BadEvent, got {other:?}"),
            Ok(_) => panic!("expected BadEvent error, got Ok"),
        }
    }
}
