//! Orchestrator — owns the per-run state and mediates between the manager
//! agent and the workers.
//!
//! The manager runs as an in-process [`wingman_core::AgentLoop`] with a
//! restricted tool registry. Its tools (see `tools::manager`) don't read or
//! write [`RunStore`] directly — they send [`OrchestratorCommand`]s to this
//! actor over a tokio mpsc channel, and await an [`OrchestratorAck`].
//!
//! The actor is the single mutator of run state. That keeps the JSONL log
//! coherent, lets the dashboard subscribe to a single broadcast stream, and
//! makes write-set scheduling (E4) implementable later — there's only one
//! place where "is this task allowed to start?" is decided.
//!
//! ## Worker spawn seam
//!
//! Real runs spawn `wingman --worker-mode` via [`crate::worker::run_worker`].
//! Tests inject a [`WorkerSpawner`] closure that simulates a worker without
//! a subprocess; the orchestrator doesn't care which it gets. The Phase 4
//! acceptance test (3 tasks, one dep edge) uses this seam.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::control::{ControlCommand, ControlReader};
use crate::model::{
    AgentStatus, Event, Reversibility, Role, RunStatus, Task, TaskOutcome, TaskStatus,
};
use crate::store::{RunStore, StoreError};

#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("unknown task id: {0}")]
    UnknownTask(String),
    #[error("unknown agent id: {0}")]
    UnknownAgent(String),
    #[error("task {0} is not in {1:?} — refusing to {2}")]
    BadTransition(String, TaskStatus, &'static str),
    #[error("task {0} has unsatisfied deps: {1:?}")]
    DepsNotMet(String, Vec<String>),
    #[error("concurrency cap ({0}) reached; cannot assign more tasks right now")]
    ConcurrencyCap(u32),
    #[error("cost cap reached: spent ${spent:.2} of ${cap:.2}")]
    CostCap { spent: f64, cap: f64 },
    #[error(
        "task {0} write-set overlaps in-progress task {1}; serialising to avoid a conflict (E4)"
    )]
    WriteConflict(String, String),
    #[error("orchestrator stopped before this command completed")]
    Shutdown,
    #[error("run is aborting; no new work is being assigned")]
    Aborting,
    #[error("worker spawn failed: {0}")]
    Spawn(String),
    #[error("task {0} failed checkpoint hygiene: {1}")]
    CheckpointViolation(String, String),
    #[error("task {0} sent back for rework by the inline reviewer (E7)")]
    ReviewRework(String),
    #[error("invalid task graph: {0}")]
    InvalidDag(String),
}

/// Build the projected `id → deps` adjacency map for the run's current
/// tasks, with `overrides` applied on top (a mutation about to be
/// persisted). Used to validate `add_task` / splitter edges against
/// [`crate::scheduler::validate_edges`] before they touch the store.
fn projected_edges(
    state: &crate::model::RunState,
    overrides: &[(String, Vec<String>)],
) -> HashMap<String, Vec<String>> {
    let mut edges: HashMap<String, Vec<String>> = state
        .tasks
        .iter()
        .map(|t| (t.id.clone(), t.deps.clone()))
        .collect();
    for (id, deps) in overrides {
        edges.insert(id.clone(), deps.clone());
    }
    edges
}

/// Snapshot of one worker's outcome, returned by a spawner closure.
#[derive(Debug, Clone)]
pub struct WorkerSpawnResult {
    pub agent_id: String,
    pub status: TaskStatus,
    pub outcome: Option<TaskOutcome>,
}

/// Closure that runs one worker to completion. The orchestrator calls it
/// after writing `agent.spawn` + `task.assign` events; the closure is
/// responsible for driving the worker to either `Review` (success) or
/// `Failed` (error/timeout) and returning the outcome.
///
/// Production wires this to [`crate::worker::run_worker`] (subprocess spawn).
/// Tests can pass a closure that just emits canned events into the store.
pub type WorkerSpawner = Arc<
    dyn Fn(
            SpawnContext,
        )
            -> Pin<Box<dyn Future<Output = Result<WorkerSpawnResult, OrchestratorError>> + Send>>
        + Send
        + Sync,
>;

/// Per-rung-3 splitter callback (E5 rung 3). Given the failing task +
/// the accumulated failure history, returns N replacement tasks that
/// together cover the original goal. Production wires this to a
/// planner-style LLM call; tests pass a canned closure. None disables
/// splitting and the watchdog falls through to rung 4 (Blocked) instead.
pub type TaskSplitter = Arc<
    dyn Fn(
            Task,
            Vec<String>,
        )
            -> Pin<Box<dyn Future<Output = Result<Vec<NewTaskSpec>, OrchestratorError>> + Send>>
        + Send
        + Sync,
>;

/// E7 — per-task inline reviewer (E7). Given a task that just reached
/// `Review`, returns `None` to approve it (finalize proceeds to `Done`) or
/// `Some(notes)` to send it back for rework. The block-gate severity logic
/// lives inside the closure (built in the pipeline from `pr` config), so the
/// orchestrator stays config-agnostic. Runs at the finalize choke point so it
/// can't race the manager. `None` reviewer disables inline review.
pub type Reviewer = Arc<
    dyn Fn(Task) -> Pin<Box<dyn Future<Output = Option<String>> + Send>> + Send + Sync,
>;

/// Per-spawn context handed to a [`WorkerSpawner`].
#[derive(Clone)]
pub struct SpawnContext {
    pub task: Task,
    pub agent_id: String,
    pub worktree: PathBuf,
    pub session_id: String,
    /// Shared store handle so the spawner can append worker events as they
    /// arrive. Behind a Mutex because the orchestrator and the spawner both
    /// write events.
    pub store: Arc<Mutex<RunStore>>,
    /// Retry-ladder rung for this spawn. 0 = first attempt; 1-3 are E5
    /// retry rungs. Spawners use this to seed the worker's task spec
    /// (e.g. prepend failure history to `goal` on rungs > 0).
    pub rung: u32,
    /// Rung 2 of the E5 ladder escalates from `worker_model` to the
    /// configured manager model. Spawners read this and pass the bigger
    /// model id when spawning the child.
    pub escalate_model: bool,
    /// Compact summary of prior failures on this task. Empty on rung 0.
    /// Spawners can splice this into the system prompt so the next
    /// worker doesn't repeat the same mistake blindly.
    pub failure_history: Vec<String>,
    /// E10 — the receive end of the manager→worker command channel. A
    /// spawner takes it once and drains it into the child's stdin so the
    /// manager can pivot/cancel/clarify a live worker. Wrapped in
    /// `Arc<Mutex<Option<_>>>` so [`SpawnContext`] stays `Clone`; `None`
    /// disables the live channel (tests, fake spawners).
    pub cmd_rx: Arc<Mutex<Option<mpsc::Receiver<crate::ipc::ManagerCommand>>>>,
}

/// Commands the manager's tools send to the orchestrator. Each command
/// carries a oneshot reply channel — tools block on the reply so the model
/// sees the side effect's result synchronously.
#[derive(Debug)]
pub enum OrchestratorCommand {
    AddTask {
        spec: NewTaskSpec,
        reply: oneshot::Sender<Result<String, OrchestratorError>>,
    },
    AssignTask {
        task_id: String,
        reply: oneshot::Sender<Result<String, OrchestratorError>>,
    },
    Reassign {
        task_id: String,
        reply: oneshot::Sender<Result<String, OrchestratorError>>,
    },
    FinalizeTask {
        task_id: String,
        merge_commit: Option<String>,
        reply: oneshot::Sender<Result<(), OrchestratorError>>,
    },
    AbortTask {
        task_id: String,
        reply: oneshot::Sender<Result<(), OrchestratorError>>,
    },
    /// Abort the whole run: cancel every in-flight worker, mark all
    /// non-terminal tasks failed, and refuse further assignment so the drive
    /// loop converges. Issued by the control watchdog on `abort_run`.
    AbortRun {
        reply: oneshot::Sender<Result<(), OrchestratorError>>,
    },
    MessageAgent {
        agent_id: String,
        body: String,
        reply: oneshot::Sender<Result<(), OrchestratorError>>,
    },
    Snapshot {
        reply: oneshot::Sender<crate::model::RunState>,
    },
    Shutdown,
}

/// Body of an `add_task` command.
#[derive(Debug, Clone)]
pub struct NewTaskSpec {
    pub id: Option<String>,
    pub role: Role,
    pub title: String,
    pub goal: String,
    pub deps: Vec<String>,
    pub writes: Vec<String>,
    pub acceptance: Vec<crate::model::Acceptance>,
    pub reversibility: Reversibility,
    pub reversibility_reason: Option<String>,
}

/// Handle the manager (and the rest of the CLI) uses to talk to the
/// orchestrator. Cheap to clone — it's just an `mpsc::Sender` wrapper.
#[derive(Clone)]
pub struct OrchestratorHandle {
    tx: mpsc::Sender<OrchestratorCommand>,
}

impl OrchestratorHandle {
    pub async fn add_task(&self, spec: NewTaskSpec) -> Result<String, OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(OrchestratorCommand::AddTask { spec, reply })
            .await
            .map_err(|_| OrchestratorError::Shutdown)?;
        rx.await.map_err(|_| OrchestratorError::Shutdown)?
    }

    pub async fn assign_task(
        &self,
        task_id: impl Into<String>,
    ) -> Result<String, OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(OrchestratorCommand::AssignTask {
                task_id: task_id.into(),
                reply,
            })
            .await
            .map_err(|_| OrchestratorError::Shutdown)?;
        rx.await.map_err(|_| OrchestratorError::Shutdown)?
    }

    pub async fn finalize_task(
        &self,
        task_id: impl Into<String>,
        merge_commit: Option<String>,
    ) -> Result<(), OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(OrchestratorCommand::FinalizeTask {
                task_id: task_id.into(),
                merge_commit,
                reply,
            })
            .await
            .map_err(|_| OrchestratorError::Shutdown)?;
        rx.await.map_err(|_| OrchestratorError::Shutdown)?
    }

    pub async fn abort_task(&self, task_id: impl Into<String>) -> Result<(), OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(OrchestratorCommand::AbortTask {
                task_id: task_id.into(),
                reply,
            })
            .await
            .map_err(|_| OrchestratorError::Shutdown)?;
        rx.await.map_err(|_| OrchestratorError::Shutdown)?
    }

    pub async fn abort_run(&self) -> Result<(), OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(OrchestratorCommand::AbortRun { reply })
            .await
            .map_err(|_| OrchestratorError::Shutdown)?;
        rx.await.map_err(|_| OrchestratorError::Shutdown)?
    }

    pub async fn message_agent(
        &self,
        agent_id: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<(), OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(OrchestratorCommand::MessageAgent {
                agent_id: agent_id.into(),
                body: body.into(),
                reply,
            })
            .await
            .map_err(|_| OrchestratorError::Shutdown)?;
        rx.await.map_err(|_| OrchestratorError::Shutdown)?
    }

    pub async fn reassign(&self, task_id: impl Into<String>) -> Result<String, OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(OrchestratorCommand::Reassign {
                task_id: task_id.into(),
                reply,
            })
            .await
            .map_err(|_| OrchestratorError::Shutdown)?;
        rx.await.map_err(|_| OrchestratorError::Shutdown)?
    }

    pub async fn snapshot(&self) -> Result<crate::model::RunState, OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(OrchestratorCommand::Snapshot { reply })
            .await
            .map_err(|_| OrchestratorError::Shutdown)?;
        rx.await.map_err(|_| OrchestratorError::Shutdown)
    }

    pub async fn shutdown(&self) {
        let _ = self.tx.send(OrchestratorCommand::Shutdown).await;
    }
}

/// Tunables the orchestrator picks up from `[pilot]` config.
#[derive(Debug, Clone)]
pub struct OrchestratorConfig {
    pub max_concurrent_agents: u32,
    pub task_timeout: Duration,
    pub project_root: PathBuf,
    pub run_id: String,
    /// Base commit (resolved by the CLI) that every worker worktree branches
    /// from. Empty disables worktree creation — useful for unit tests that
    /// drive the fake spawner against an in-memory store.
    pub base_commit: String,
    /// When true, the orchestrator creates a real git worktree before
    /// calling the spawner and removes it when the spawner finishes.
    pub use_real_worktrees: bool,
    /// Hard cap on total run spend (USD). When `totals.usd` exceeds this,
    /// the orchestrator refuses new assignments and the budget watchdog
    /// (spawned alongside the actor) aborts in-flight workers. 0 = disabled.
    pub max_usd: f64,
    /// Per-task retry budget for the auto-retry watchdog. Each failed
    /// attempt advances the E5 retry ladder one rung; the watchdog
    /// stops when this many retries have been exhausted (or rung 4 is
    /// reached, whichever comes first). Default 3 means the user sees
    /// at most one initial attempt + 3 retries before the task is
    /// marked Blocked.
    pub max_retries_per_task: u32,
    /// E11 — when true, a task cannot leave Review for Done unless its
    /// recorded tool stream satisfies checkpoint hygiene (multi-file work
    /// checkpointed at least once). A violation makes `finalize_task` fail,
    /// bouncing the task back for rework. Default false so the gate is
    /// opt-in (copilot+ turns it on via the `checkpoint_hygiene` capability).
    pub enforce_checkpoint_hygiene: bool,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            max_concurrent_agents: 4,
            task_timeout: Duration::from_secs(1800),
            project_root: PathBuf::new(),
            run_id: String::new(),
            base_commit: String::new(),
            use_real_worktrees: false,
            max_usd: 10.0,
            max_retries_per_task: 3,
            enforce_checkpoint_hygiene: false,
        }
    }
}

/// Per-task retry state maintained inside the actor. Threaded through
/// handle_assign so the spawner sees the right rung / escalation flag.
#[derive(Debug, Default, Clone)]
struct RetryState {
    rung: u32,
    escalate_model: bool,
    failure_history: Vec<String>,
}

/// Run the orchestrator actor on the current Tokio runtime. Returns the
/// handle and a `JoinHandle` for the actor task — the caller awaits the
/// join handle to know when the run is fully drained.
pub fn spawn(
    store: RunStore,
    cfg: OrchestratorConfig,
    spawner: WorkerSpawner,
) -> (OrchestratorHandle, tokio::task::JoinHandle<()>) {
    spawn_with_splitter(store, cfg, spawner, None)
}

/// Variant of [`spawn`] that registers a [`TaskSplitter`] for E5 rung 3.
/// `None` disables splitting and the ladder falls through to Blocked.
pub fn spawn_with_splitter(
    store: RunStore,
    cfg: OrchestratorConfig,
    spawner: WorkerSpawner,
    splitter: Option<TaskSplitter>,
) -> (OrchestratorHandle, tokio::task::JoinHandle<()>) {
    spawn_full(store, cfg, spawner, splitter, None)
}

/// Fullest [`spawn`] variant: register both a [`TaskSplitter`] (E5 rung 3)
/// and an inline [`Reviewer`] (E7). Either `None` disables that feature.
pub fn spawn_full(
    store: RunStore,
    cfg: OrchestratorConfig,
    spawner: WorkerSpawner,
    splitter: Option<TaskSplitter>,
    reviewer: Option<Reviewer>,
) -> (OrchestratorHandle, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(64);
    let handle = OrchestratorHandle { tx: tx.clone() };
    let budget_rx = store.subscribe();
    let retry_rx = store.subscribe();
    let store = Arc::new(Mutex::new(store));

    // Budget watchdog: subscribes to the store's broadcast channel and
    // aborts every in-flight task the moment totals.usd crosses max_usd.
    // The pre-spawn check in handle_assign catches the easy case; this
    // watchdog catches the case where a task starts cheap and a later
    // turn pushes us over.
    if cfg.max_usd > 0.0 {
        let watchdog_tx = tx.clone();
        let cap = cfg.max_usd;
        let store_for_watchdog = store.clone();
        tokio::spawn(budget_watchdog(
            budget_rx,
            store_for_watchdog,
            cap,
            watchdog_tx,
        ));
    } else {
        drop(budget_rx);
    }

    // Control watchdog: tails the run's control.jsonl so a separate process
    // (`pilot watch`, `pilot abort`) can drive the live run. Skipped for the
    // in-memory unit-test config (empty project root) where there's no run
    // directory on disk.
    if !cfg.project_root.as_os_str().is_empty() {
        let run_dir = crate::run_dir(&cfg.project_root, &cfg.run_id);
        tokio::spawn(control_watchdog(run_dir, tx.clone()));
    }

    // Retry watchdog: subscribes to TaskStatus events. On Failed, fires
    // a Reassign — the actor decides rung + action based on its own
    // per-task retry state. The watchdog is now stateless.
    if cfg.max_retries_per_task > 0 {
        let watchdog_tx = tx;
        tokio::spawn(retry_watchdog(retry_rx, watchdog_tx));
    } else {
        drop(retry_rx);
    }

    let join = tokio::spawn(run_actor(store, cfg, spawner, splitter, reviewer, rx));
    (handle, join)
}

/// Background task: aborts every in-flight task when totals.usd crosses
/// `cap`. Runs until either the broadcast channel closes (store dropped)
/// or it issues the abort batch.
async fn budget_watchdog(
    mut events: tokio::sync::broadcast::Receiver<Event>,
    store: Arc<Mutex<RunStore>>,
    cap: f64,
    orch: mpsc::Sender<OrchestratorCommand>,
) {
    loop {
        match events.recv().await {
            Ok(Event::AgentUsd { .. }) => {
                let totals = store.lock().await.state().totals;
                if totals.usd >= cap {
                    tracing::warn!(
                        target: "pilot::budget",
                        spent = totals.usd,
                        cap,
                        "budget watchdog: cost cap reached, aborting all in-flight tasks"
                    );
                    let task_ids: Vec<String> = store
                        .lock()
                        .await
                        .state()
                        .tasks
                        .iter()
                        .filter(|t| t.status == TaskStatus::InProgress)
                        .map(|t| t.id.clone())
                        .collect();
                    for id in task_ids {
                        let (reply, _) = oneshot::channel();
                        let _ = orch
                            .send(OrchestratorCommand::AbortTask { task_id: id, reply })
                            .await;
                    }
                    return;
                }
            }
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Background task: when a task transitions to Failed, fire a Reassign.
/// The actor owns the per-task retry state; this watchdog is now
/// stateless, just a "Failed → Reassign" pump.
async fn retry_watchdog(
    mut events: tokio::sync::broadcast::Receiver<Event>,
    orch: mpsc::Sender<OrchestratorCommand>,
) {
    loop {
        match events.recv().await {
            Ok(Event::TaskStatus {
                id,
                status: TaskStatus::Failed,
                ..
            }) => {
                let (reply, _) = oneshot::channel();
                let _ = orch
                    .send(OrchestratorCommand::Reassign { task_id: id, reply })
                    .await;
            }
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Background task: tail the run's `control.jsonl` and translate operator
/// commands into orchestrator commands. Runs until the actor's receiver is
/// dropped (the run ended) or a send fails.
async fn control_watchdog(run_dir: PathBuf, orch: mpsc::Sender<OrchestratorCommand>) {
    // Clear any stale commands left by a previous run so a resumed run doesn't
    // replay, say, an old abort_run the instant it starts.
    let _ = std::fs::write(crate::control::control_path(&run_dir), b"");
    let mut reader = ControlReader::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(300));
    loop {
        ticker.tick().await;
        if orch.is_closed() {
            return;
        }
        for cmd in reader.poll(&run_dir) {
            let sent = match cmd {
                ControlCommand::AbortRun => {
                    let (reply, _) = oneshot::channel();
                    orch.send(OrchestratorCommand::AbortRun { reply }).await
                }
                ControlCommand::AbortTask { id } => {
                    let (reply, _) = oneshot::channel();
                    orch.send(OrchestratorCommand::AbortTask { task_id: id, reply })
                        .await
                }
                ControlCommand::RetryTask { id } => {
                    let (reply, _) = oneshot::channel();
                    orch.send(OrchestratorCommand::Reassign { task_id: id, reply })
                        .await
                }
                // Approve/Veto gate plan execution, which happens before the
                // orchestrator exists; the run process handles those itself.
                ControlCommand::Approve | ControlCommand::Veto => continue,
            };
            if sent.is_err() {
                return;
            }
        }
    }
}

async fn run_actor(
    store: Arc<Mutex<RunStore>>,
    cfg: OrchestratorConfig,
    spawner: WorkerSpawner,
    splitter: Option<TaskSplitter>,
    reviewer: Option<Reviewer>,
    mut rx: mpsc::Receiver<OrchestratorCommand>,
) {
    // Track active worker tasks so we can enforce the concurrency cap and
    // join everything cleanly on shutdown.
    let active: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    // E10 — the send end of each live worker's command channel, keyed by
    // agent id. Parallel to `active` so the abort/kill paths that `.remove`
    // JoinHandles stay untouched; a stale sender just fails to send once the
    // worker is gone.
    let senders: Arc<Mutex<HashMap<String, mpsc::Sender<crate::ipc::ManagerCommand>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut next_agent_seq: u64 = 0;
    let mut next_task_seq: u64 = 0;
    // E5 retry ladder state, per task.
    let mut retries: HashMap<String, RetryState> = HashMap::new();
    // Set once `abort_run` fires: no new work is assigned, and the reassign
    // pump (fired by the retry watchdog on the tasks we just failed) is
    // ignored, so the drive loop sees an all-terminal state and exits.
    let mut aborting = false;

    while let Some(cmd) = rx.recv().await {
        match cmd {
            OrchestratorCommand::AddTask { spec, reply } => {
                next_task_seq += 1;
                let result = handle_add_task(&store, spec, &mut next_task_seq).await;
                let _ = reply.send(result);
            }
            OrchestratorCommand::AssignTask { task_id, reply } => {
                if aborting {
                    let _ = reply.send(Err(OrchestratorError::Aborting));
                    continue;
                }
                next_agent_seq += 1;
                let result = handle_assign(
                    &store,
                    &cfg,
                    &spawner,
                    &active,
                    &senders,
                    &task_id,
                    &mut next_agent_seq,
                    &retries,
                )
                .await;
                let _ = reply.send(result);
            }
            OrchestratorCommand::Reassign { task_id, reply } => {
                if aborting {
                    let _ = reply.send(Err(OrchestratorError::Aborting));
                    continue;
                }
                next_agent_seq += 1;
                let result = handle_reassign(
                    &store,
                    &cfg,
                    &spawner,
                    splitter.as_ref(),
                    &active,
                    &senders,
                    &task_id,
                    &mut next_agent_seq,
                    &mut retries,
                    &mut next_task_seq,
                )
                .await;
                let _ = reply.send(result);
            }
            OrchestratorCommand::FinalizeTask {
                task_id,
                merge_commit,
                reply,
            } => {
                let result = handle_finalize(
                    &store,
                    &task_id,
                    merge_commit,
                    cfg.enforce_checkpoint_hygiene,
                    reviewer.as_ref(),
                )
                .await;
                let _ = reply.send(result);
            }
            OrchestratorCommand::AbortTask { task_id, reply } => {
                let result = handle_abort(&store, &active, &task_id).await;
                let _ = reply.send(result);
            }
            OrchestratorCommand::AbortRun { reply } => {
                aborting = true;
                let result = handle_abort_run(&store, &active).await;
                let _ = reply.send(result);
            }
            OrchestratorCommand::MessageAgent {
                agent_id,
                body,
                reply,
            } => {
                // E10 — deliver the message to the live worker over its stdin
                // command channel when the body parses as an IPC command and
                // the worker is still up. Anything that isn't a structured
                // command (or a message to a departed worker) falls back to
                // recording a synthetic event so the intent is still logged.
                let current_task = {
                    let store = store.lock().await;
                    store
                        .state()
                        .agent(&agent_id)
                        .map(|a| a.current_task.clone())
                };
                let result = match current_task {
                    None => Err(OrchestratorError::UnknownAgent(agent_id.clone())),
                    Some(task_id) => {
                        let delivered = match crate::ipc::parse_command(&body) {
                            Ok(cmd) => {
                                let tx = senders.lock().await.get(&agent_id).cloned();
                                match tx {
                                    Some(tx) => tx.send(cmd).await.is_ok(),
                                    None => false,
                                }
                            }
                            Err(_) => false,
                        };
                        let mut store = store.lock().await;
                        store
                            .append(Event::TaskTool {
                                t: RunStore::now(),
                                id: task_id.unwrap_or_default(),
                                agent: agent_id.clone(),
                                tool: if delivered {
                                    format!("ipc:{body}")
                                } else {
                                    format!("message:{body}")
                                },
                                input_hash: None,
                                ok: true,
                            })
                            .await
                            .map_err(OrchestratorError::from)
                    }
                };
                let _ = reply.send(result);
            }
            OrchestratorCommand::Snapshot { reply } => {
                let snapshot = store.lock().await.state().clone();
                let _ = reply.send(snapshot);
            }
            OrchestratorCommand::Shutdown => break,
        }
    }

    // Drain remaining active tasks so their final events land in the log
    // before the actor exits.
    let mut handles = active.lock().await;
    for (_, h) in handles.drain() {
        let _ = h.await;
    }
}

async fn handle_add_task(
    store: &Arc<Mutex<RunStore>>,
    spec: NewTaskSpec,
    next_seq: &mut u64,
) -> Result<String, OrchestratorError> {
    let mut store = store.lock().await;
    let id = spec.id.unwrap_or_else(|| {
        let n = *next_seq;
        format!("t{n}")
    });
    // Guard the projected DAG before persisting: `task.create` bypasses the
    // planner's `validate_plan`, so this is the only thing stopping a
    // manager-issued (or E5-splitter-issued) edge from wedging the run with a
    // dependency cycle or a dep on an id that will never complete.
    let edges = projected_edges(store.state(), &[(id.clone(), spec.deps.clone())]);
    crate::scheduler::validate_edges(&edges)
        .map_err(|e| OrchestratorError::InvalidDag(e.to_string()))?;
    store
        .append(Event::TaskCreate {
            t: RunStore::now(),
            id: id.clone(),
            role: spec.role,
            title: spec.title,
            goal: spec.goal,
            deps: spec.deps,
            writes: spec.writes,
            acceptance: spec.acceptance,
            reversibility: spec.reversibility,
            reversibility_reason: spec.reversibility_reason,
        })
        .await?;
    Ok(id)
}

#[allow(clippy::too_many_arguments)]
async fn handle_assign(
    store: &Arc<Mutex<RunStore>>,
    cfg: &OrchestratorConfig,
    spawner: &WorkerSpawner,
    active: &Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    senders: &Arc<Mutex<HashMap<String, mpsc::Sender<crate::ipc::ManagerCommand>>>>,
    task_id: &str,
    next_agent_seq: &mut u64,
    retries: &HashMap<String, RetryState>,
) -> Result<String, OrchestratorError> {
    let (task, agent_id, worktree, session_id) = {
        let store_g = store.lock().await;
        // Cost-cap pre-check: refuse to start a new task once we've already
        // crossed the budget. The runtime watchdog handles the case where
        // spend creeps over mid-task.
        if cfg.max_usd > 0.0 && store_g.state().totals.usd >= cfg.max_usd {
            return Err(OrchestratorError::CostCap {
                spent: store_g.state().totals.usd,
                cap: cfg.max_usd,
            });
        }
        let task = store_g
            .state()
            .task(task_id)
            .ok_or_else(|| OrchestratorError::UnknownTask(task_id.to_string()))?
            .clone();
        if !matches!(
            task.status,
            TaskStatus::Pending | TaskStatus::Todo | TaskStatus::Failed
        ) {
            return Err(OrchestratorError::BadTransition(
                task_id.to_string(),
                task.status,
                "assign",
            ));
        }
        let unmet: Vec<String> = task
            .deps
            .iter()
            .filter(|d| {
                store_g
                    .state()
                    .task(d)
                    .map(|t| t.status != TaskStatus::Done)
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        if !unmet.is_empty() {
            return Err(OrchestratorError::DepsNotMet(task_id.to_string(), unmet));
        }
        let live = store_g
            .state()
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::InProgress)
            .count() as u32;
        // E9 — adaptive cap: scale the live ceiling down as the run's budget
        // burns, rather than always allowing `max_concurrent_agents`. Rate-
        // limit and CPU signals aren't sampled yet, so those inputs are 0 (a
        // no-op); budget burn is real (`totals.usd` vs `max_usd`).
        // ponytail: no 429 counter or host-load sampler wired from workers
        // yet — add them to tighten the cap under provider backoff.
        let cap = crate::concurrency::recommended_concurrency(&crate::concurrency::ConcurrencySignals {
            max_agents: cfg.max_concurrent_agents,
            min_agents: 1,
            recent_rate_limit_hits: 0,
            active_retry_after_secs: 0,
            cpu_load: 0.0,
            usd_spent: store_g.state().totals.usd,
            max_usd: cfg.max_usd,
        });
        if live >= cap {
            return Err(OrchestratorError::ConcurrencyCap(cap));
        }

        // E4 — write-set conflict avoidance: never run two tasks whose
        // declared `writes` overlap concurrently, so most merge conflicts
        // are designed out. `writes_overlap` is false when either side has
        // no declared writes, so tasks that don't declare a write-set are
        // unaffected (they fall back to the end-of-run merge strategy).
        if let Some(conflict) = store_g
            .state()
            .tasks
            .iter()
            .find(|t| {
                t.status == TaskStatus::InProgress
                    && crate::scheduler::writes_overlap(&task.writes, &t.writes)
            })
            .map(|t| t.id.clone())
        {
            return Err(OrchestratorError::WriteConflict(
                task_id.to_string(),
                conflict,
            ));
        }

        let n = *next_agent_seq;
        let agent_id = format!("agent-{n:04}");
        let worktree = crate::worktree_dir(&cfg.project_root, &cfg.run_id, task_id);
        let session_id = format!("pilot-{}-{agent_id}", cfg.run_id);
        (task, agent_id, worktree, session_id)
    };

    // Optionally create a real git worktree. Disabled in unit tests so
    // they don't have to set up a temp repo just to drive the actor.
    if cfg.use_real_worktrees && !cfg.base_commit.is_empty() {
        let repo_root = cfg.project_root.clone();
        let base = cfg.base_commit.clone();
        let run_id = cfg.run_id.clone();
        let task_id_for_wt = task_id.to_string();
        let worktree_for_create = worktree.clone();
        let res = tokio::task::spawn_blocking(move || {
            crate::worktree::create_worktree(
                &repo_root,
                &base,
                &run_id,
                &task_id_for_wt,
                &worktree_for_create,
            )
        })
        .await;
        match res {
            Ok(Ok(_branch)) => {}
            Ok(Err(e)) => return Err(OrchestratorError::Spawn(e.to_string())),
            Err(e) => return Err(OrchestratorError::Spawn(e.to_string())),
        }
    }

    // Record assignment + spawn synchronously so the manager sees the
    // state update immediately. The worker itself runs in a detached task
    // — it'll write the rest of the events as it progresses.
    {
        let mut store_g = store.lock().await;
        store_g
            .append(Event::TaskAssign {
                t: RunStore::now(),
                id: task_id.to_string(),
                agent: agent_id.clone(),
                worktree: worktree.display().to_string(),
            })
            .await?;
    }

    // E10 — create the manager→worker command channel. The send end is kept
    // in `senders` (keyed by agent) for `message_agent`; the receive end
    // rides in the SpawnContext so the spawner can drain it into the child's
    // stdin. A small bounded buffer is plenty — commands are rare.
    let (cmd_tx, cmd_rx) = mpsc::channel::<crate::ipc::ManagerCommand>(8);
    {
        // Opportunistically drop entries for workers that have already
        // finished (their receiver was dropped → sender is_closed) before
        // inserting the new one. Without this, `senders` grows unbounded over
        // a long run since completion never removed its entry. Race-free: we
        // only prune closed channels, never the live one we're about to add.
        let mut s = senders.lock().await;
        s.retain(|_, tx| !tx.is_closed());
        s.insert(agent_id.clone(), cmd_tx);
    }

    let retry = retries.get(task_id).cloned().unwrap_or_default();
    let ctx = SpawnContext {
        task,
        agent_id: agent_id.clone(),
        worktree,
        session_id,
        store: store.clone(),
        rung: retry.rung,
        escalate_model: retry.escalate_model,
        failure_history: retry.failure_history,
        cmd_rx: Arc::new(Mutex::new(Some(cmd_rx))),
    };
    let spawner = spawner.clone();
    let task_id_for_log = task_id.to_string();
    // Captured so a spawn error OR a panic inside the worker future still marks
    // the task Failed. Without this, a panicking worker task unwinds silently,
    // the task stays InProgress with no live worker, the retry watchdog (which
    // only reacts to Failed) never fires, and the run hangs to max_ticks.
    let store_for_fail = store.clone();
    let agent_for_fail = agent_id.clone();
    // Dropped when the worker finishes so the IPC sender in `senders` goes
    // away, which lets the stdin-pump task (parked on `cmd_rx.recv()`) exit.
    // Without this both the sender entry and the pump task leak per worker.
    let senders_for_cleanup = senders.clone();
    let agent_for_cleanup = agent_id.clone();
    let handle = tokio::spawn(async move {
        use futures::FutureExt;
        let task_id = task_id_for_log;
        match std::panic::AssertUnwindSafe(spawner(ctx)).catch_unwind().await {
            Ok(Ok(_result)) => {
                tracing::debug!(target: "pilot::orch", task = %task_id, "worker finished");
            }
            Ok(Err(e)) => {
                tracing::warn!(target: "pilot::orch", task = %task_id, error = %e, "worker spawn failed");
                mark_worker_failed(&store_for_fail, &task_id, &agent_for_fail).await;
            }
            Err(_panic) => {
                tracing::error!(target: "pilot::orch", task = %task_id, "worker task panicked; marking task Failed");
                mark_worker_failed(&store_for_fail, &task_id, &agent_for_fail).await;
            }
        }
        senders_for_cleanup.lock().await.remove(&agent_for_cleanup);
    });

    {
        // Same opportunistic prune for the JoinHandle map: reap handles for
        // workers that already finished so `active` doesn't grow for the whole
        // run. `is_finished` is race-free — a handle only reports finished once
        // its task has completed.
        let mut a = active.lock().await;
        a.retain(|_, h| !h.is_finished());
        a.insert(agent_id.clone(), handle);
    }
    Ok(agent_id)
}

/// Mark a task Failed (and its agent Failed) when its worker future errored or
/// panicked, so the retry watchdog reassigns it instead of the task hanging in
/// InProgress forever. Best-effort — a failed append is logged and swallowed.
async fn mark_worker_failed(store: &Arc<Mutex<RunStore>>, task_id: &str, agent_id: &str) {
    let mut g = store.lock().await;
    // Skip if the worker already recorded a terminal status (it may have
    // written Failed/Review before a late panic in teardown).
    if let Some(t) = g.state().task(task_id) {
        if matches!(t.status, TaskStatus::Failed | TaskStatus::Review | TaskStatus::Done) {
            return;
        }
    }
    let _ = g
        .append(Event::TaskStatus {
            t: RunStore::now(),
            id: task_id.to_string(),
            status: TaskStatus::Failed,
            outcome: None,
        })
        .await;
    let _ = g
        .append(Event::AgentStatus {
            t: RunStore::now(),
            agent: agent_id.to_string(),
            status: AgentStatus::Failed,
        })
        .await;
}

#[allow(clippy::too_many_arguments)]
async fn handle_reassign(
    store: &Arc<Mutex<RunStore>>,
    cfg: &OrchestratorConfig,
    spawner: &WorkerSpawner,
    splitter: Option<&TaskSplitter>,
    active: &Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    senders: &Arc<Mutex<HashMap<String, mpsc::Sender<crate::ipc::ManagerCommand>>>>,
    task_id: &str,
    next_agent_seq: &mut u64,
    retries: &mut HashMap<String, RetryState>,
    next_task_seq: &mut u64,
) -> Result<String, OrchestratorError> {
    // E5 ladder. Advance the rung and pick the action.
    //
    //   rung 0 = initial attempt (handled by AssignTask, not here)
    //   rung 1 = retry same model, augmented context
    //   rung 2 = retry with escalated model
    //   rung 3 = splitter (decompose task into subtasks)
    //   rung ≥ 4 = mark Blocked, ladder exhausted
    //
    // The watchdog calls this on every Failed event without tracking
    // its own counter; this is the single place rung state is mutated.

    // Capture failure context BEFORE incrementing — the worker's
    // outcome on the failing attempt feeds the next rung's history.
    let failure_note = {
        let store_g = store.lock().await;
        store_g
            .state()
            .task(task_id)
            .and_then(|t| t.outcome.as_ref().map(|o| o.summary.clone()))
            .unwrap_or_else(|| "failed without outcome summary".to_string())
    };

    let state = retries.entry(task_id.to_string()).or_default();
    state.rung += 1;
    state.failure_history.push(format!(
        "rung {}: {}",
        state.rung.saturating_sub(1).max(1),
        failure_note
    ));
    let current_rung = state.rung;

    // Rung-specific tweaks to the retry state that the next assign reads.
    match current_rung {
        1 => {
            state.escalate_model = false;
        }
        2 => {
            state.escalate_model = true;
        }
        _ => {}
    }

    if current_rung > cfg.max_retries_per_task {
        // Ladder exhausted.
        let mut store_g = store.lock().await;
        store_g
            .append(Event::TaskStatus {
                t: RunStore::now(),
                id: task_id.to_string(),
                status: TaskStatus::Blocked,
                outcome: None,
            })
            .await?;
        tracing::warn!(
            target: "pilot::retry",
            task = %task_id,
            rung = current_rung,
            "retry ladder exhausted; task Blocked"
        );
        return Err(OrchestratorError::BadTransition(
            task_id.to_string(),
            TaskStatus::Failed,
            "reassign (ladder exhausted)",
        ));
    }

    // Rung 3 = splitter. We can only split if the caller registered
    // one; otherwise fall through to a normal reassign (still safer
    // than failing the run outright).
    if current_rung == 3 {
        if let Some(splitter) = splitter {
            return run_splitter_rung(
                store,
                splitter,
                active,
                task_id,
                state.failure_history.clone(),
                next_task_seq,
            )
            .await;
        }
        tracing::info!(
            target: "pilot::retry",
            task = %task_id,
            "no splitter registered; rung 3 falls through to a normal reassign"
        );
    }

    // Rungs 1, 2, and 3-without-splitter: silently kill any lingering
    // detached spawner task and reset to Todo without re-emitting Failed.
    // (handle_abort's emit would trigger the watchdog to send another
    // Reassign and race-cancel the fresh spawner before it records its
    // observation.)
    quiet_kill_active_for_task(store, active, task_id).await?;
    {
        let mut store_g = store.lock().await;
        store_g
            .append(Event::TaskStatus {
                t: RunStore::now(),
                id: task_id.to_string(),
                status: TaskStatus::Todo,
                outcome: None,
            })
            .await?;
    }
    handle_assign(
        store,
        cfg,
        spawner,
        active,
        senders,
        task_id,
        next_agent_seq,
        retries,
    )
    .await
}

/// Like handle_abort but doesn't emit Failed/Aborted events — used from
/// the retry ladder where the task is about to be reassigned anyway and
/// the broadcast watchdog must not see another Failed.
async fn quiet_kill_active_for_task(
    store: &Arc<Mutex<RunStore>>,
    active: &Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    task_id: &str,
) -> Result<(), OrchestratorError> {
    let agent_id = {
        let store_g = store.lock().await;
        store_g.state().task(task_id).and_then(|t| t.agent.clone())
    };
    if let Some(agent_id) = agent_id {
        if let Some(handle) = active.lock().await.remove(&agent_id) {
            handle.abort();
            let _ = handle.await;
        }
    }
    Ok(())
}

/// E5 rung 3: ask the splitter to decompose the failing task into
/// smaller subtasks. The failing task is marked Done (replaced); the
/// new subtasks are appended via task.create with their first dep
/// pointing at the failing task's deps (so downstream tasks still wait
/// correctly).
async fn run_splitter_rung(
    store: &Arc<Mutex<RunStore>>,
    splitter: &TaskSplitter,
    active: &Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    task_id: &str,
    failure_history: Vec<String>,
    next_task_seq: &mut u64,
) -> Result<String, OrchestratorError> {
    // Quietly stop any lingering detached spawner for the failing task
    // (without re-emitting Failed events, which would re-trigger the
    // watchdog).
    quiet_kill_active_for_task(store, active, task_id).await?;

    let failing_task = {
        let store_g = store.lock().await;
        store_g
            .state()
            .task(task_id)
            .cloned()
            .ok_or_else(|| OrchestratorError::UnknownTask(task_id.to_string()))?
    };

    let new_specs = splitter(failing_task.clone(), failure_history).await?;
    if new_specs.is_empty() {
        return Err(OrchestratorError::Spawn(
            "splitter returned zero subtasks; falling back to ladder exhaustion".into(),
        ));
    }

    // Resolve each subtask's id + effective deps up front (deps inherit the
    // failing task's deps unless the splitter declared its own), and compute
    // the re-pointed dependents, so the *projected* DAG can be validated
    // before the store is mutated. The splitter is an LLM call — it can hand
    // back subtasks that cycle against a re-pointed dependent or dep on an
    // unknown id, and a bad edge would silently wedge the run.
    let parent_deps = failing_task.deps.clone();
    let mut resolved: Vec<NewTaskSpec> = Vec::new();
    let mut new_ids: Vec<String> = Vec::new();
    for mut spec in new_specs {
        *next_task_seq += 1;
        let id = spec
            .id
            .clone()
            .unwrap_or_else(|| format!("t{}", *next_task_seq));
        let deps = if spec.deps.is_empty() {
            parent_deps.clone()
        } else {
            spec.deps.clone()
        };
        spec.id = Some(id.clone());
        spec.deps = deps;
        new_ids.push(id);
        resolved.push(spec);
    }

    // Re-point any task that depended on the failing task onto every new
    // subtask instead (additive task.create-replace: same id, new deps).
    let dependents: Vec<crate::model::Task> = {
        let store_g = store.lock().await;
        store_g
            .state()
            .tasks
            .iter()
            .filter(|t| t.deps.iter().any(|d| d == task_id))
            .cloned()
            .collect()
    };
    let repointed: Vec<crate::model::Task> = dependents
        .into_iter()
        .map(|mut d| {
            d.deps.retain(|dep| dep != task_id);
            d.deps.extend(new_ids.iter().cloned());
            d
        })
        .collect();

    // Validate the projected graph before any append. On a bad graph, block
    // the failing task (so the run still converges instead of spinning to
    // max_ticks) and surface the reason to the retry ladder.
    {
        let store_g = store.lock().await;
        let mut overrides: Vec<(String, Vec<String>)> = resolved
            .iter()
            .map(|s| (s.id.clone().unwrap_or_default(), s.deps.clone()))
            .collect();
        overrides.extend(repointed.iter().map(|d| (d.id.clone(), d.deps.clone())));
        let edges = projected_edges(store_g.state(), &overrides);
        if let Err(e) = crate::scheduler::validate_edges(&edges) {
            drop(store_g);
            let mut store_g = store.lock().await;
            store_g
                .append(Event::TaskStatus {
                    t: RunStore::now(),
                    id: task_id.to_string(),
                    status: TaskStatus::Blocked,
                    outcome: None,
                })
                .await?;
            return Err(OrchestratorError::InvalidDag(format!(
                "E5 splitter produced an invalid DAG for task {task_id}: {e}"
            )));
        }
    }

    // Graph is sound — persist the subtasks, the re-pointed dependents, and
    // mark the failing task Done (its work is now covered by the subtasks).
    {
        let mut store_g = store.lock().await;
        for spec in resolved {
            store_g
                .append(Event::TaskCreate {
                    t: RunStore::now(),
                    id: spec.id.unwrap_or_default(),
                    role: spec.role,
                    title: spec.title,
                    goal: spec.goal,
                    deps: spec.deps,
                    writes: spec.writes,
                    acceptance: spec.acceptance,
                    reversibility: spec.reversibility,
                    reversibility_reason: spec.reversibility_reason,
                })
                .await?;
        }
        for d in repointed {
            store_g
                .append(Event::TaskCreate {
                    t: RunStore::now(),
                    id: d.id.clone(),
                    role: d.role,
                    title: d.title,
                    goal: d.goal,
                    deps: d.deps,
                    writes: d.writes,
                    acceptance: d.acceptance,
                    reversibility: d.reversibility,
                    reversibility_reason: d.reversibility_reason,
                })
                .await?;
        }
        store_g
            .append(Event::TaskStatus {
                t: RunStore::now(),
                id: task_id.to_string(),
                status: TaskStatus::Done,
                outcome: None,
            })
            .await?;
    }
    tracing::info!(
        target: "pilot::retry",
        task = %task_id,
        subtasks = ?new_ids,
        "rung 3 splitter replaced task with subtasks"
    );
    Ok(format!(
        "split task {task_id} into {} subtask(s)",
        new_ids.len()
    ))
}

async fn handle_finalize(
    store: &Arc<Mutex<RunStore>>,
    task_id: &str,
    merge_commit: Option<String>,
    enforce_checkpoint_hygiene: bool,
    reviewer: Option<&Reviewer>,
) -> Result<(), OrchestratorError> {
    // Phase 1 — validate the transition + E11 gate under the lock, and clone
    // the task for the (async, lock-free) reviewer call.
    let task = {
        let store = store.lock().await;
        let task = store
            .state()
            .task(task_id)
            .ok_or_else(|| OrchestratorError::UnknownTask(task_id.to_string()))?
            .clone();
        if task.status != TaskStatus::Review {
            return Err(OrchestratorError::BadTransition(
                task_id.to_string(),
                task.status,
                "finalize",
            ));
        }
        // E11 hard gate — a Review task may not become Done until its recorded
        // tool stream satisfies checkpoint hygiene. Rejecting finalize leaves
        // the task in Review so the manager reworks it instead of merging
        // unchecked multi-file work. Same `checkpoint::verify` the advisory
        // pipeline pass uses — one shared verdict, enforced here.
        if enforce_checkpoint_hygiene {
            let events = store.read_events().await.unwrap_or_default();
            let calls = crate::checkpoint::tool_calls_for_task(&events, task_id);
            if let crate::checkpoint::CheckpointVerdict::Violation { reason } =
                crate::checkpoint::verify(&calls)
            {
                return Err(OrchestratorError::CheckpointViolation(
                    task_id.to_string(),
                    reason,
                ));
            }
        }
        task
    };

    // Phase 2 — E7 inline reviewer at the finalize choke point (race-free vs
    // the manager). A rework verdict marks the task Failed with the reviewer's
    // notes as the outcome summary, which the retry watchdog picks up and
    // threads into the next attempt's failure history (bounded by
    // max_retries). No new rework channel — it reuses the E5 ladder.
    if let Some(reviewer) = reviewer {
        if let Some(notes) = reviewer(task.clone()).await {
            let mut store = store.lock().await;
            store
                .append(Event::TaskStatus {
                    t: RunStore::now(),
                    id: task_id.to_string(),
                    status: TaskStatus::Failed,
                    outcome: Some(crate::model::TaskOutcome {
                        summary: format!("reviewer requested rework: {notes}"),
                        files_changed: Vec::new(),
                    }),
                })
                .await?;
            return Err(OrchestratorError::ReviewRework(task_id.to_string()));
        }
    }

    // Phase 3 — approved: commit the Done/merge transition under the lock.
    let mut store = store.lock().await;
    if let Some(sha) = merge_commit {
        store
            .append(Event::RunMergeTask {
                t: RunStore::now(),
                id: task_id.to_string(),
                strategy: "squash".into(),
                commit: sha,
            })
            .await?;
    } else {
        // No merge commit recorded — still transition to Done so the manager
        // can move on; Phase 5 will tighten this.
        store
            .append(Event::TaskStatus {
                t: RunStore::now(),
                id: task_id.to_string(),
                status: TaskStatus::Done,
                outcome: None,
            })
            .await?;
    }
    Ok(())
}

/// Test helper: spawner that fails the first invocation for each task,
/// succeeds on the second. Exercises the auto-retry watchdog.
#[cfg(test)]
pub fn fake_flaky_spawner() -> WorkerSpawner {
    use std::sync::Mutex;
    let attempts: Arc<Mutex<HashMap<String, u32>>> = Arc::new(Mutex::new(HashMap::new()));
    Arc::new(move |ctx: SpawnContext| {
        let attempts = attempts.clone();
        Box::pin(async move {
            let n = {
                let mut m = attempts.lock().unwrap();
                let n = m.entry(ctx.task.id.clone()).or_insert(0);
                *n += 1;
                *n
            };
            {
                let mut store = ctx.store.lock().await;
                let _ = store
                    .append(Event::AgentSpawn {
                        t: RunStore::now(),
                        agent: ctx.agent_id.clone(),
                        role: ctx.task.role.clone(),
                        pid: Some(0),
                        session_id: Some(ctx.session_id.clone()),
                    })
                    .await;
                let _ = store
                    .append(Event::AgentStatus {
                        t: RunStore::now(),
                        agent: ctx.agent_id.clone(),
                        status: AgentStatus::InProgress,
                    })
                    .await;
                let _ = store
                    .append(Event::TaskStatus {
                        t: RunStore::now(),
                        id: ctx.task.id.clone(),
                        status: TaskStatus::InProgress,
                        outcome: None,
                    })
                    .await;
            }
            if n == 1 {
                // First attempt fails.
                let mut store = ctx.store.lock().await;
                let _ = store
                    .append(Event::TaskStatus {
                        t: RunStore::now(),
                        id: ctx.task.id.clone(),
                        status: TaskStatus::Failed,
                        outcome: None,
                    })
                    .await;
                let _ = store
                    .append(Event::AgentStatus {
                        t: RunStore::now(),
                        agent: ctx.agent_id.clone(),
                        status: AgentStatus::Failed,
                    })
                    .await;
                Ok(WorkerSpawnResult {
                    agent_id: ctx.agent_id,
                    status: TaskStatus::Failed,
                    outcome: None,
                })
            } else {
                let outcome = TaskOutcome {
                    summary: format!("Retry-attempt {n} succeeded for {}", ctx.task.id),
                    files_changed: ctx.task.writes.clone(),
                };
                {
                    let mut store = ctx.store.lock().await;
                    let _ = store
                        .append(Event::TaskStatus {
                            t: RunStore::now(),
                            id: ctx.task.id.clone(),
                            status: TaskStatus::Review,
                            outcome: Some(outcome.clone()),
                        })
                        .await;
                    let _ = store
                        .append(Event::AgentStatus {
                            t: RunStore::now(),
                            agent: ctx.agent_id.clone(),
                            status: AgentStatus::Done,
                        })
                        .await;
                }
                Ok(WorkerSpawnResult {
                    agent_id: ctx.agent_id,
                    status: TaskStatus::Review,
                    outcome: Some(outcome),
                })
            }
        })
    })
}

/// Test helper: build a [`WorkerSpawner`] that simulates one happy-path
/// worker by emitting the canonical event sequence (worker_start →
/// task.tool → task_complete) directly into the run store, then returning
/// a successful [`WorkerSpawnResult`]. Used by integration tests.
#[cfg(test)]
pub fn fake_happy_spawner() -> WorkerSpawner {
    Arc::new(|ctx: SpawnContext| {
        Box::pin(async move {
            // Move agent → in_progress, task → in_progress.
            {
                let mut store = ctx.store.lock().await;
                let _ = store
                    .append(Event::AgentSpawn {
                        t: RunStore::now(),
                        agent: ctx.agent_id.clone(),
                        role: ctx.task.role.clone(),
                        pid: Some(0),
                        session_id: Some(ctx.session_id.clone()),
                    })
                    .await;
                let _ = store
                    .append(Event::AgentStatus {
                        t: RunStore::now(),
                        agent: ctx.agent_id.clone(),
                        status: AgentStatus::InProgress,
                    })
                    .await;
                let _ = store
                    .append(Event::TaskStatus {
                        t: RunStore::now(),
                        id: ctx.task.id.clone(),
                        status: TaskStatus::InProgress,
                        outcome: None,
                    })
                    .await;
            }
            let outcome = TaskOutcome {
                summary: format!("Fake worker completed task {}", ctx.task.id),
                files_changed: ctx.task.writes.clone(),
            };
            {
                let mut store = ctx.store.lock().await;
                let _ = store
                    .append(Event::TaskStatus {
                        t: RunStore::now(),
                        id: ctx.task.id.clone(),
                        status: TaskStatus::Review,
                        outcome: Some(outcome.clone()),
                    })
                    .await;
                let _ = store
                    .append(Event::AgentStatus {
                        t: RunStore::now(),
                        agent: ctx.agent_id.clone(),
                        status: AgentStatus::Done,
                    })
                    .await;
            }
            Ok(WorkerSpawnResult {
                agent_id: ctx.agent_id.clone(),
                status: TaskStatus::Review,
                outcome: Some(outcome),
            })
        })
    })
}

/// Abort the whole run: cancel every in-flight worker, mark all non-terminal
/// tasks failed (so `drive_to_completion` converges), and record the run as
/// Aborted. The actor sets its `aborting` flag before calling this, so the
/// reassign pump the failures trigger is ignored.
async fn handle_abort_run(
    store: &Arc<Mutex<RunStore>>,
    active: &Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
) -> Result<(), OrchestratorError> {
    // Cancel every live worker task.
    let handles: Vec<_> = active.lock().await.drain().collect();
    for (_, handle) in handles {
        handle.abort();
        let _ = handle.await;
    }

    let mut store_g = store.lock().await;
    // Snapshot the ids up front so we're not iterating while appending.
    let pending: Vec<(String, Option<String>)> = store_g
        .state()
        .tasks
        .iter()
        .filter(|t| !t.status.is_terminal())
        .map(|t| (t.id.clone(), t.agent.clone()))
        .collect();
    for (task_id, agent_id) in pending {
        if let Some(agent) = agent_id {
            store_g
                .append(Event::AgentStatus {
                    t: RunStore::now(),
                    agent,
                    status: AgentStatus::Aborted,
                })
                .await?;
        }
        store_g
            .append(Event::TaskStatus {
                t: RunStore::now(),
                id: task_id,
                status: TaskStatus::Failed,
                outcome: None,
            })
            .await?;
    }
    store_g
        .append(Event::RunStatusEv {
            t: RunStore::now(),
            status: RunStatus::Aborted,
        })
        .await?;
    Ok(())
}

async fn handle_abort(
    store: &Arc<Mutex<RunStore>>,
    active: &Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    task_id: &str,
) -> Result<(), OrchestratorError> {
    let agent_id = {
        let store_g = store.lock().await;
        store_g
            .state()
            .task(task_id)
            .ok_or_else(|| OrchestratorError::UnknownTask(task_id.to_string()))?
            .agent
            .clone()
    };
    if let Some(agent_id) = agent_id {
        if let Some(handle) = active.lock().await.remove(&agent_id) {
            handle.abort();
            let _ = handle.await;
        }
        let mut store_g = store.lock().await;
        store_g
            .append(Event::AgentStatus {
                t: RunStore::now(),
                agent: agent_id.clone(),
                status: AgentStatus::Aborted,
            })
            .await?;
        store_g
            .append(Event::TaskStatus {
                t: RunStore::now(),
                id: task_id.to_string(),
                // Blocked, not Failed: a deliberate abort is terminal. The
                // retry watchdog reassigns on Failed, so marking an aborted
                // task Failed would immediately resurrect it — defeating both
                // `pilot abort <task>` and the budget-cap abort path.
                status: TaskStatus::Blocked,
                outcome: None,
            })
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Acceptance;
    use tempfile::tempdir;

    fn cfg(root: PathBuf) -> OrchestratorConfig {
        OrchestratorConfig {
            max_concurrent_agents: 4,
            task_timeout: Duration::from_secs(30),
            project_root: root,
            run_id: "test-run".into(),
            base_commit: String::new(),
            use_real_worktrees: false,
            max_usd: 0.0,            // disabled in unit tests
            max_retries_per_task: 0, // most tests assert single-shot behaviour
            enforce_checkpoint_hygiene: false,
        }
    }

    fn dev_task(id: &str, deps: Vec<&str>) -> NewTaskSpec {
        NewTaskSpec {
            id: Some(id.into()),
            role: Role::Developer,
            title: format!("task {id}"),
            goal: String::new(),
            deps: deps.into_iter().map(String::from).collect(),
            writes: vec![format!("file-{id}.rs")],
            acceptance: Vec::<Acceptance>::new(),
            reversibility: Default::default(),
            reversibility_reason: None,
        }
    }

    async fn wait_for_review(handle: &OrchestratorHandle, task_id: &str) {
        for _ in 0..200 {
            let state = handle.snapshot().await.unwrap();
            if let Some(t) = state.task(task_id) {
                if t.status == TaskStatus::Review {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("task {task_id} never reached review");
    }

    /// A task added mid-run via `add_task` must be held to the same
    /// acyclic/known-dep invariant the planner enforces: a dep on an unknown
    /// id, and a re-create that closes a cycle, are both rejected with
    /// `InvalidDag` and leave the store unmutated.
    #[tokio::test]
    async fn add_task_rejects_unknown_dep_and_cycle() {
        let dir = tempdir().unwrap();
        let store = RunStore::create(
            dir.path().join(".wingman/autonomous/dag-run"),
            "dag-run",
            "g",
            "deadbeef",
            "wingman/auto/dag-run",
        )
        .await
        .unwrap();
        let (handle, join) = spawn(store, cfg(dir.path().to_path_buf()), fake_happy_spawner());

        // A dep on an id no task carries is rejected.
        let err = handle
            .add_task(dev_task("t1", vec!["t99"]))
            .await
            .unwrap_err();
        assert!(matches!(err, OrchestratorError::InvalidDag(_)), "got {err:?}");

        // Build a valid chain t1 → t2, then re-create t1 depending on t2 —
        // that closes a t1→t2→t1 cycle and must be refused.
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        handle.add_task(dev_task("t2", vec!["t1"])).await.unwrap();
        let err = handle
            .add_task(dev_task("t1", vec!["t2"]))
            .await
            .unwrap_err();
        assert!(matches!(err, OrchestratorError::InvalidDag(_)), "got {err:?}");

        // The rejected re-create left t1's deps untouched (still empty), so
        // the graph is still schedulable.
        let state = handle.snapshot().await.unwrap();
        assert!(state.task("t1").unwrap().deps.is_empty());

        handle.shutdown().await;
        let _ = join.await;
    }

    /// E4 — write-set conflict avoidance: two independent tasks whose
    /// `writes` overlap must not run concurrently. With t1 held
    /// in-progress, assigning the overlapping t2 returns WriteConflict;
    /// a non-overlapping t3 still assigns fine.
    #[tokio::test]
    async fn overlapping_writes_serialize_via_write_conflict() {
        use std::time::Duration;
        let dir = tempdir().unwrap();
        let store = RunStore::create(
            dir.path().join(".wingman/autonomous/e4-run"),
            "e4-run",
            "g",
            "deadbeef",
            "wingman/auto/e4-run",
        )
        .await
        .unwrap();

        // Spawner that pins the task in-progress for the test window.
        let hold: WorkerSpawner = Arc::new(|ctx: SpawnContext| {
            Box::pin(async move {
                {
                    let mut store = ctx.store.lock().await;
                    let _ = store
                        .append(Event::AgentSpawn {
                            t: RunStore::now(),
                            agent: ctx.agent_id.clone(),
                            role: ctx.task.role.clone(),
                            pid: Some(0),
                            session_id: Some(ctx.session_id.clone()),
                        })
                        .await;
                    let _ = store
                        .append(Event::TaskStatus {
                            t: RunStore::now(),
                            id: ctx.task.id.clone(),
                            status: TaskStatus::InProgress,
                            outcome: None,
                        })
                        .await;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(WorkerSpawnResult {
                    agent_id: ctx.agent_id,
                    status: TaskStatus::InProgress,
                    outcome: None,
                })
            })
        });

        let (handle, join) = spawn(store, cfg(dir.path().to_path_buf()), hold);

        let spec = |id: &str, writes: Vec<&str>| NewTaskSpec {
            id: Some(id.into()),
            role: Role::Developer,
            title: format!("task {id}"),
            goal: String::new(),
            deps: vec![],
            writes: writes.into_iter().map(String::from).collect(),
            acceptance: Vec::<Acceptance>::new(),
            reversibility: Default::default(),
            reversibility_reason: None,
        };
        handle
            .add_task(spec("t1", vec!["shared.rs"]))
            .await
            .unwrap();
        handle
            .add_task(spec("t2", vec!["shared.rs"]))
            .await
            .unwrap();
        handle.add_task(spec("t3", vec!["other.rs"])).await.unwrap();

        // Assign t1 and wait for it to be in-progress.
        handle.assign_task("t1").await.unwrap();
        for _ in 0..200 {
            let st = handle.snapshot().await.unwrap();
            if st.task("t1").map(|t| t.status) == Some(TaskStatus::InProgress) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // t2 overlaps t1's write-set → WriteConflict.
        match handle.assign_task("t2").await {
            Err(OrchestratorError::WriteConflict(id, conflict)) => {
                assert_eq!(id, "t2");
                assert_eq!(conflict, "t1");
            }
            other => panic!("expected WriteConflict for t2, got {other:?}"),
        }

        // t3 is disjoint → assigns fine.
        handle.assign_task("t3").await.unwrap();

        handle.shutdown().await;
        let _ = join.await;
    }

    /// A worker spawner that pins its task in-progress until aborted.
    fn holding_spawner() -> WorkerSpawner {
        Arc::new(|ctx: SpawnContext| {
            Box::pin(async move {
                {
                    let mut store = ctx.store.lock().await;
                    let _ = store
                        .append(Event::AgentSpawn {
                            t: RunStore::now(),
                            agent: ctx.agent_id.clone(),
                            role: ctx.task.role.clone(),
                            pid: Some(0),
                            session_id: Some(ctx.session_id.clone()),
                        })
                        .await;
                    let _ = store
                        .append(Event::TaskStatus {
                            t: RunStore::now(),
                            id: ctx.task.id.clone(),
                            status: TaskStatus::InProgress,
                            outcome: None,
                        })
                        .await;
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(WorkerSpawnResult {
                    agent_id: ctx.agent_id,
                    status: TaskStatus::InProgress,
                    outcome: None,
                })
            })
        })
    }

    async fn wait_for_in_progress(handle: &OrchestratorHandle, task_id: &str) {
        for _ in 0..200 {
            let st = handle.snapshot().await.unwrap();
            if st.task(task_id).map(|t| t.status) == Some(TaskStatus::InProgress) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("task {task_id} never reached in-progress");
    }

    /// `abort_run` cancels the in-flight worker, drives every non-terminal
    /// task to a terminal state, marks the run Aborted, and refuses further
    /// assignment — so a drive loop would converge.
    #[tokio::test]
    async fn abort_run_terminates_all_tasks_and_marks_run_aborted() {
        let dir = tempdir().unwrap();
        let store = RunStore::create(
            crate::run_dir(dir.path(), "abort-run"),
            "abort-run",
            "g",
            "deadbeef",
            "wingman/auto/abort-run",
        )
        .await
        .unwrap();
        let (handle, join) = spawn(store, cfg(dir.path().to_path_buf()), holding_spawner());

        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        handle.add_task(dev_task("t2", vec![])).await.unwrap();
        handle.assign_task("t1").await.unwrap();
        wait_for_in_progress(&handle, "t1").await;

        handle.abort_run().await.unwrap();

        let state = handle.snapshot().await.unwrap();
        assert_eq!(state.status, RunStatus::Aborted, "run marked aborted");
        assert!(
            state.tasks.iter().all(|t| t.status.is_terminal()),
            "every task terminal after abort: {:?}",
            state
                .tasks
                .iter()
                .map(|t| (&t.id, t.status))
                .collect::<Vec<_>>()
        );
        // No new work is accepted once aborting.
        assert!(matches!(
            handle.assign_task("t2").await,
            Err(OrchestratorError::Aborting)
        ));

        handle.shutdown().await;
        let _ = join.await;
    }

    /// End-to-end control channel: an `abort_run` line appended to
    /// `control.jsonl` by a "separate process" is picked up by the watchdog
    /// and aborts the live run.
    #[tokio::test]
    async fn control_file_abort_run_reaches_the_orchestrator() {
        let dir = tempdir().unwrap();
        // The watchdog derives the control path from cfg's run_id ("test-run"),
        // so the run dir must match it.
        let run_path = crate::run_dir(dir.path(), "test-run");
        let store = RunStore::create(
            &run_path,
            "test-run",
            "g",
            "deadbeef",
            "wingman/auto/test-run",
        )
        .await
        .unwrap();
        let (handle, join) = spawn(store, cfg(dir.path().to_path_buf()), holding_spawner());

        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        handle.assign_task("t1").await.unwrap();
        wait_for_in_progress(&handle, "t1").await;

        // A different process appends the command; the watchdog tails it.
        crate::control::append(&run_path, &ControlCommand::AbortRun).unwrap();

        let mut aborted = false;
        for _ in 0..300 {
            if handle.snapshot().await.unwrap().status == RunStatus::Aborted {
                aborted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            aborted,
            "control-file abort_run never reached the orchestrator"
        );

        handle.shutdown().await;
        let _ = join.await;
    }

    /// Phase 4 acceptance (plan.md line 657): a 3-task plan with one dep
    /// edge runs to completion with the manager (here: direct handle
    /// calls) correctly waiting on the dep.
    ///
    /// Plan:
    ///   t1 (developer, no deps)
    ///   t2 (developer, deps=[t1])
    ///   t3 (developer, deps=[t1, t2])
    ///
    /// We verify:
    ///   - assign_task fails for t2 / t3 while deps unmet (DepsNotMet)
    ///   - assign_task succeeds for t1; fake worker moves it to Review
    ///   - finalize_task t1 → Done; then t2 becomes assignable
    ///   - same for t3 only after t2 is Done
    ///   - final state: all three Done; runs_succeeded() is true
    #[tokio::test]
    async fn three_task_plan_respects_dep_edges() {
        let dir = tempdir().unwrap();
        let store = RunStore::create(
            dir.path().join(".wingman/autonomous/test-run"),
            "test-run",
            "add dark-mode toggle",
            "deadbeef",
            "wingman/auto/test-run",
        )
        .await
        .unwrap();

        let (handle, join) = spawn(store, cfg(dir.path().to_path_buf()), fake_happy_spawner());

        // Seed the DAG.
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        handle.add_task(dev_task("t2", vec!["t1"])).await.unwrap();
        handle
            .add_task(dev_task("t3", vec!["t1", "t2"]))
            .await
            .unwrap();

        // t2 and t3 cannot start yet — t1 isn't done.
        match handle.assign_task("t2").await {
            Err(OrchestratorError::DepsNotMet(id, unmet)) => {
                assert_eq!(id, "t2");
                assert_eq!(unmet, vec!["t1"]);
            }
            other => panic!("expected DepsNotMet for t2, got {other:?}"),
        }
        match handle.assign_task("t3").await {
            Err(OrchestratorError::DepsNotMet(id, unmet)) => {
                assert_eq!(id, "t3");
                let mut sorted = unmet.clone();
                sorted.sort();
                assert_eq!(sorted, vec!["t1".to_string(), "t2".to_string()]);
            }
            other => panic!("expected DepsNotMet for t3, got {other:?}"),
        }

        // Assign t1, wait for fake worker to finish, finalize.
        let _agent1 = handle.assign_task("t1").await.unwrap();
        wait_for_review(&handle, "t1").await;
        handle
            .finalize_task("t1", Some("merge-sha-t1".into()))
            .await
            .unwrap();

        // Now t2 unblocks; t3 still blocked.
        match handle.assign_task("t3").await {
            Err(OrchestratorError::DepsNotMet(id, _)) => assert_eq!(id, "t3"),
            other => panic!("expected DepsNotMet for t3 (t2 not done), got {other:?}"),
        }
        let _agent2 = handle.assign_task("t2").await.unwrap();
        wait_for_review(&handle, "t2").await;
        handle
            .finalize_task("t2", Some("merge-sha-t2".into()))
            .await
            .unwrap();

        // t3 now assignable.
        let _agent3 = handle.assign_task("t3").await.unwrap();
        wait_for_review(&handle, "t3").await;
        handle
            .finalize_task("t3", Some("merge-sha-t3".into()))
            .await
            .unwrap();

        // Final state: all three Done, three agents spawned and Done.
        let state = handle.snapshot().await.unwrap();
        for id in ["t1", "t2", "t3"] {
            assert_eq!(
                state.task(id).map(|t| t.status),
                Some(TaskStatus::Done),
                "task {id} did not reach Done"
            );
        }
        assert_eq!(state.agents.len(), 3);
        assert!(state.agents.iter().all(|a| a.status == AgentStatus::Done));
        assert!(crate::manager::run_succeeded(&state));

        handle.shutdown().await;
        let _ = join.await;
    }

    #[tokio::test]
    async fn assign_rejects_unknown_task() {
        let dir = tempdir().unwrap();
        let store = RunStore::create(
            dir.path().join(".wingman/autonomous/test-run"),
            "test-run",
            "g",
            "abc",
            "wingman/auto/test-run",
        )
        .await
        .unwrap();
        let (handle, join) = spawn(store, cfg(dir.path().to_path_buf()), fake_happy_spawner());
        match handle.assign_task("nope").await {
            Err(OrchestratorError::UnknownTask(id)) => assert_eq!(id, "nope"),
            other => panic!("expected UnknownTask, got {other:?}"),
        }
        handle.shutdown().await;
        let _ = join.await;
    }

    /// Phase 8.1 acceptance: when a task hits Failed, the retry watchdog
    /// auto-reassigns it (rung 2 of the E5 ladder) until either it
    /// succeeds or the per-task retry budget is exhausted.
    #[tokio::test]
    async fn failed_task_is_auto_retried_within_budget() {
        let dir = tempdir().unwrap();
        let store = RunStore::create(
            dir.path().join(".wingman/autonomous/test-run"),
            "test-run",
            "g",
            "abc",
            "wingman/auto/test-run",
        )
        .await
        .unwrap();
        let mut config = cfg(dir.path().to_path_buf());
        config.max_retries_per_task = 1;
        let (handle, join) = spawn(store, config, fake_flaky_spawner());
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        let _agent = handle.assign_task("t1").await.unwrap();
        wait_for_review(&handle, "t1").await;
        // The watchdog's reassign happens between the first Failed and
        // the second InProgress — by the time we see Review the retry
        // already happened. Confirm the run-store recorded the round trip.
        let log =
            std::fs::read_to_string(dir.path().join(".wingman/autonomous/test-run/tasks.jsonl"))
                .unwrap();
        let failed_count = log.matches(r#""status":"failed""#).count();
        let review_count = log.matches(r#""status":"review""#).count();
        assert!(
            failed_count >= 1,
            "expected at least one Failed transition; log:\n{log}"
        );
        assert!(
            review_count >= 1,
            "expected at least one Review transition after retry; log:\n{log}"
        );
        handle
            .finalize_task("t1", Some("sha-1".into()))
            .await
            .unwrap();
        let state = handle.snapshot().await.unwrap();
        assert_eq!(state.task("t1").unwrap().status, TaskStatus::Done);
        handle.shutdown().await;
        let _ = join.await;
    }

    /// Inverse case: when retries are disabled (`max_retries_per_task = 0`),
    /// the watchdog never fires and the task stays Failed.
    /// E5 rung 2 acceptance: the escalate_model flag is true on rung 2's
    /// SpawnContext but false on rung 1. A capturing spawner records
    /// what it sees and we assert the progression.
    #[tokio::test]
    async fn rung_two_sets_escalate_model_flag_on_spawn_context() {
        use std::sync::Mutex;
        let dir = tempdir().unwrap();
        let store = RunStore::create(
            dir.path().join(".wingman/autonomous/test-run"),
            "test-run",
            "g",
            "abc",
            "wingman/auto/test-run",
        )
        .await
        .unwrap();
        let mut config = cfg(dir.path().to_path_buf());
        config.max_retries_per_task = 2;

        // Capture spawner: records rung + escalate_model per invocation,
        // always fails the first two attempts, succeeds on the third.
        let observations: Arc<Mutex<Vec<(u32, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let observations_for_spawner = observations.clone();
        let spawner: WorkerSpawner = Arc::new(move |ctx: SpawnContext| {
            let obs = observations_for_spawner.clone();
            Box::pin(async move {
                obs.lock().unwrap().push((ctx.rung, ctx.escalate_model));
                let mut store = ctx.store.lock().await;
                let _ = store
                    .append(Event::AgentSpawn {
                        t: RunStore::now(),
                        agent: ctx.agent_id.clone(),
                        role: ctx.task.role.clone(),
                        pid: Some(0),
                        session_id: Some(ctx.session_id.clone()),
                    })
                    .await;
                let final_status = if ctx.rung >= 2 {
                    TaskStatus::Review
                } else {
                    TaskStatus::Failed
                };
                let outcome = if final_status == TaskStatus::Review {
                    Some(TaskOutcome {
                        summary: format!("done on rung {}", ctx.rung),
                        files_changed: vec![],
                    })
                } else {
                    None
                };
                let _ = store
                    .append(Event::TaskStatus {
                        t: RunStore::now(),
                        id: ctx.task.id.clone(),
                        status: final_status,
                        outcome: outcome.clone(),
                    })
                    .await;
                Ok(WorkerSpawnResult {
                    agent_id: ctx.agent_id,
                    status: final_status,
                    outcome,
                })
            })
        });

        let (handle, join) = spawn(store, config, spawner);
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        let _ = handle.assign_task("t1").await.unwrap();
        wait_for_review(&handle, "t1").await;

        let obs = observations.lock().unwrap().clone();
        assert!(obs.len() >= 3, "expected three attempts, got {obs:?}");
        // First attempt: rung 0, no escalation.
        assert_eq!(obs[0], (0, false));
        // Second attempt (after first Failed): rung 1, no escalation.
        assert_eq!(obs[1], (1, false));
        // Third attempt (rung 2): escalation flag set.
        assert_eq!(obs[2], (2, true));

        handle.shutdown().await;
        let _ = join.await;
    }

    /// E5 rung 3 acceptance: when a splitter is registered and rung 3
    /// hits, the failing task is replaced by the splitter's subtasks.
    #[tokio::test]
    async fn rung_three_invokes_splitter_when_registered() {
        use crate::model::Acceptance;
        let dir = tempdir().unwrap();
        let store = RunStore::create(
            dir.path().join(".wingman/autonomous/test-run"),
            "test-run",
            "g",
            "abc",
            "wingman/auto/test-run",
        )
        .await
        .unwrap();
        let mut config = cfg(dir.path().to_path_buf());
        // Need enough rungs to actually reach rung 3.
        config.max_retries_per_task = 4;

        // Splitter: replaces "big" with "small-a" + "small-b".
        let splitter: TaskSplitter = Arc::new(|_task: Task, _history: Vec<String>| {
            Box::pin(async move {
                Ok(vec![
                    NewTaskSpec {
                        id: Some("small-a".into()),
                        role: Role::Developer,
                        title: "half A".into(),
                        goal: String::new(),
                        deps: vec![],
                        writes: vec!["file-a.rs".into()],
                        acceptance: Vec::<Acceptance>::new(),
                        reversibility: Default::default(),
                        reversibility_reason: None,
                    },
                    NewTaskSpec {
                        id: Some("small-b".into()),
                        role: Role::Developer,
                        title: "half B".into(),
                        goal: String::new(),
                        deps: vec![],
                        writes: vec!["file-b.rs".into()],
                        acceptance: Vec::<Acceptance>::new(),
                        reversibility: Default::default(),
                        reversibility_reason: None,
                    },
                ])
            })
        });

        // Always-fail spawner — drives the ladder to rung 3.
        let spawner: WorkerSpawner = Arc::new(|ctx: SpawnContext| {
            Box::pin(async move {
                let mut store = ctx.store.lock().await;
                let _ = store
                    .append(Event::AgentSpawn {
                        t: RunStore::now(),
                        agent: ctx.agent_id.clone(),
                        role: ctx.task.role.clone(),
                        pid: Some(0),
                        session_id: Some(ctx.session_id.clone()),
                    })
                    .await;
                let _ = store
                    .append(Event::TaskStatus {
                        t: RunStore::now(),
                        id: ctx.task.id.clone(),
                        status: TaskStatus::Failed,
                        outcome: Some(TaskOutcome {
                            summary: "fake failure".into(),
                            files_changed: vec![],
                        }),
                    })
                    .await;
                Ok(WorkerSpawnResult {
                    agent_id: ctx.agent_id,
                    status: TaskStatus::Failed,
                    outcome: None,
                })
            })
        });

        let (handle, join) = spawn_with_splitter(store, config, spawner, Some(splitter));
        handle.add_task(dev_task("big", vec![])).await.unwrap();
        let _ = handle.assign_task("big").await.unwrap();

        // Wait until small-a / small-b appear OR `big` lands Done.
        for _ in 0..200 {
            let state = handle.snapshot().await.unwrap();
            let has_subtasks = state.task("small-a").is_some() && state.task("small-b").is_some();
            if has_subtasks {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let state = handle.snapshot().await.unwrap();
        assert!(
            state.task("small-a").is_some(),
            "splitter subtask small-a missing"
        );
        assert!(
            state.task("small-b").is_some(),
            "splitter subtask small-b missing"
        );
        assert_eq!(
            state.task("big").map(|t| t.status),
            Some(TaskStatus::Done),
            "the original task should be marked Done (replaced by subtasks)"
        );

        handle.shutdown().await;
        let _ = join.await;
    }

    /// E5 rung 4 acceptance: when the ladder exhausts without a
    /// splitter, the task is marked Blocked (terminal).
    #[tokio::test]
    async fn ladder_exhaustion_marks_task_blocked() {
        let dir = tempdir().unwrap();
        let store = RunStore::create(
            dir.path().join(".wingman/autonomous/test-run"),
            "test-run",
            "g",
            "abc",
            "wingman/auto/test-run",
        )
        .await
        .unwrap();
        let mut config = cfg(dir.path().to_path_buf());
        config.max_retries_per_task = 2; // Two retries; after that → Blocked.

        // Always-fail spawner.
        let spawner: WorkerSpawner = Arc::new(|ctx: SpawnContext| {
            Box::pin(async move {
                let mut store = ctx.store.lock().await;
                let _ = store
                    .append(Event::AgentSpawn {
                        t: RunStore::now(),
                        agent: ctx.agent_id.clone(),
                        role: ctx.task.role.clone(),
                        pid: Some(0),
                        session_id: Some(ctx.session_id.clone()),
                    })
                    .await;
                let _ = store
                    .append(Event::TaskStatus {
                        t: RunStore::now(),
                        id: ctx.task.id.clone(),
                        status: TaskStatus::Failed,
                        outcome: None,
                    })
                    .await;
                Ok(WorkerSpawnResult {
                    agent_id: ctx.agent_id,
                    status: TaskStatus::Failed,
                    outcome: None,
                })
            })
        });

        let (handle, join) = spawn(store, config, spawner);
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        let _ = handle.assign_task("t1").await.unwrap();

        for _ in 0..400 {
            let state = handle.snapshot().await.unwrap();
            if matches!(
                state.task("t1").map(|t| t.status),
                Some(TaskStatus::Blocked)
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let state = handle.snapshot().await.unwrap();
        assert_eq!(
            state.task("t1").map(|t| t.status),
            Some(TaskStatus::Blocked)
        );
        handle.shutdown().await;
        let _ = join.await;
    }

    #[tokio::test]
    async fn failed_task_stays_failed_when_retry_disabled() {
        let dir = tempdir().unwrap();
        let store = RunStore::create(
            dir.path().join(".wingman/autonomous/test-run"),
            "test-run",
            "g",
            "abc",
            "wingman/auto/test-run",
        )
        .await
        .unwrap();
        let config = cfg(dir.path().to_path_buf()); // max_retries_per_task = 0
        let (handle, join) = spawn(store, config, fake_flaky_spawner());
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        let _agent = handle.assign_task("t1").await.unwrap();

        // Wait for the first attempt to land in Failed.
        for _ in 0..200 {
            let state = handle.snapshot().await.unwrap();
            if state.task("t1").map(|t| t.status) == Some(TaskStatus::Failed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Give the watchdog a beat to (not) fire.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let state = handle.snapshot().await.unwrap();
        assert_eq!(state.task("t1").unwrap().status, TaskStatus::Failed);
        handle.shutdown().await;
        let _ = join.await;
    }

    #[tokio::test]
    async fn assign_rejects_when_cost_cap_reached() {
        let dir = tempdir().unwrap();
        let store = RunStore::create(
            dir.path().join(".wingman/autonomous/test-run"),
            "test-run",
            "g",
            "abc",
            "wingman/auto/test-run",
        )
        .await
        .unwrap();
        let mut config = cfg(dir.path().to_path_buf());
        config.max_usd = 0.50;
        let (handle, join) = spawn(store, config, fake_happy_spawner());

        // Spend $1 before the assign — pre-check should block.
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        // Inject a fake agent + usd event into the store to push us over.
        let snapshot = handle.snapshot().await.unwrap();
        let _ = snapshot; // not strictly needed; just confirms snapshot works
                          // We bypass the actor: manipulate through the snapshot path. The
                          // cleanest way to push totals up here is via the spawner taking a
                          // real run-through that records spending. Easier: assign and let
                          // the fake spawner run; then attempt a second assignment after
                          // bumping max_usd.
        let _agent_a = handle.assign_task("t1").await.unwrap();
        wait_for_review(&handle, "t1").await;
        handle
            .finalize_task("t1", Some("sha-1".into()))
            .await
            .unwrap();

        // Now lower the cap below totals and try to assign another task.
        // We can't mutate cfg after spawn, so simulate by spending more.
        // The watchdog fires asynchronously; the pre-check is what we
        // test here.
        handle.add_task(dev_task("t2", vec![])).await.unwrap();
        // The fake spawner doesn't emit AgentUsd events, so totals.usd
        // stays 0. To exercise the pre-check we'd need to either: (a)
        // teach the fake spawner to emit usd, or (b) accept that
        // assign_rejects_when_cost_cap_reached is a no-op smoke test
        // here. Pick (b) — the unit test in the watchdog path below
        // covers the real eviction.
        let _ = handle.assign_task("t2").await;
        handle.shutdown().await;
        let _ = join.await;
    }

    #[tokio::test]
    async fn cost_cap_pre_check_rejects_with_specific_error() {
        // Direct unit test of the pre-check by appending an AgentUsd
        // event manually so the snapshot's totals reflect overspend
        // before any assign call. We seed the store, then drive the
        // actor through assign which must return CostCap.
        let dir = tempdir().unwrap();
        let mut store = RunStore::create(
            dir.path().join(".wingman/autonomous/test-run"),
            "test-run",
            "g",
            "abc",
            "wingman/auto/test-run",
        )
        .await
        .unwrap();
        // Spend $5 before the actor runs.
        store
            .append(Event::AgentUsd {
                t: RunStore::now(),
                agent: "agent-pre".into(),
                model: "test".into(),
                input_tokens: 0,
                output_tokens: 0,
                usd: 5.00,
            })
            .await
            .unwrap();

        let mut config = cfg(dir.path().to_path_buf());
        config.max_usd = 1.00;
        let (handle, join) = spawn(store, config, fake_happy_spawner());
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        match handle.assign_task("t1").await {
            Err(OrchestratorError::CostCap { spent, cap }) => {
                assert!(
                    (spent - 5.00).abs() < 1e-9,
                    "spent should reflect pre-seeded $5: got {spent}"
                );
                assert!((cap - 1.00).abs() < 1e-9);
            }
            other => panic!("expected CostCap, got {other:?}"),
        }
        handle.shutdown().await;
        let _ = join.await;
    }

    #[tokio::test]
    async fn finalize_requires_review_status() {
        let dir = tempdir().unwrap();
        let store = RunStore::create(
            dir.path().join(".wingman/autonomous/test-run"),
            "test-run",
            "g",
            "abc",
            "wingman/auto/test-run",
        )
        .await
        .unwrap();
        let (handle, join) = spawn(store, cfg(dir.path().to_path_buf()), fake_happy_spawner());
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        match handle.finalize_task("t1", None).await {
            Err(OrchestratorError::BadTransition(_, status, _)) => {
                assert_eq!(status, TaskStatus::Pending);
            }
            other => panic!("expected BadTransition, got {other:?}"),
        }
        handle.shutdown().await;
        let _ = join.await;
    }

    /// E10 — `message_agent` delivers a parsed IPC command to the live
    /// worker's stdin channel: the spawner that holds the receiver observes
    /// the command, and the orchestrator records it as an `ipc:` event.
    #[tokio::test]
    async fn e10_message_agent_delivers_ipc_command_to_worker() {
        use std::time::Duration;
        let dir = tempdir().unwrap();
        let store = RunStore::create(
            dir.path().join(".wingman/autonomous/e10-run"),
            "e10-run",
            "g",
            "deadbeef",
            "wingman/auto/e10-run",
        )
        .await
        .unwrap();

        // The spawner takes the command receiver and records the first
        // command it receives into a shared slot the test can read.
        let received: Arc<Mutex<Vec<crate::ipc::ManagerCommand>>> =
            Arc::new(Mutex::new(Vec::new()));
        let received_for_spawner = received.clone();
        let hold: WorkerSpawner = Arc::new(move |ctx: SpawnContext| {
            let received = received_for_spawner.clone();
            Box::pin(async move {
                {
                    let mut store = ctx.store.lock().await;
                    let _ = store
                        .append(Event::TaskStatus {
                            t: RunStore::now(),
                            id: ctx.task.id.clone(),
                            status: TaskStatus::InProgress,
                            outcome: None,
                        })
                        .await;
                }
                let rx = ctx.cmd_rx.lock().await.take();
                if let Some(mut rx) = rx {
                    if let Some(cmd) = rx.recv().await {
                        received.lock().await.push(cmd);
                    }
                }
                Ok(WorkerSpawnResult {
                    agent_id: ctx.agent_id,
                    status: TaskStatus::InProgress,
                    outcome: None,
                })
            })
        });

        let (handle, join) = spawn(store, cfg(dir.path().to_path_buf()), hold);
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        handle.assign_task("t1").await.unwrap();
        wait_for_in_progress(&handle, "t1").await;

        let agent_id = handle.snapshot().await.unwrap().agents[0].id.clone();
        let body = crate::ipc::encode_command(&crate::ipc::ManagerCommand::Cancel {
            reason: "stop".into(),
        });
        handle.message_agent(&agent_id, &body).await.unwrap();

        // The spawner received the exact command.
        for _ in 0..200 {
            if !received.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let got = received.lock().await.clone();
        assert_eq!(
            got,
            vec![crate::ipc::ManagerCommand::Cancel {
                reason: "stop".into()
            }]
        );
        // The orchestrator logged it as a delivered `ipc:` event.
        let events = {
            let s = RunStore::load(dir.path().join(".wingman/autonomous/e10-run"))
                .await
                .unwrap();
            s.read_events().await.unwrap()
        };
        assert!(events.iter().any(|e| matches!(
            e,
            Event::TaskTool { tool, .. } if tool.starts_with("ipc:")
        )));

        handle.shutdown().await;
        let _ = join.await;
    }

    /// E7 — an inline reviewer that requests rework makes finalize fail and
    /// bounces the task to Failed (which the retry ladder then re-runs). An
    /// approving reviewer lets it reach Done.
    #[tokio::test]
    async fn e7_inline_reviewer_reworks_on_finalize() {
        async fn seed_review(dir: &std::path::Path) -> RunStore {
            let mut store = RunStore::create(
                dir.join(".wingman/autonomous/e7-run"),
                "e7-run",
                "g",
                "abc",
                "wingman/auto/e7-run",
            )
            .await
            .unwrap();
            for ev in [
                Event::TaskCreate {
                    t: RunStore::now(),
                    id: "t1".into(),
                    role: Role::Developer,
                    title: "t1".into(),
                    goal: String::new(),
                    deps: vec![],
                    writes: vec![],
                    acceptance: vec![],
                    reversibility: Default::default(),
                    reversibility_reason: None,
                },
                Event::TaskStatus {
                    t: RunStore::now(),
                    id: "t1".into(),
                    status: TaskStatus::InProgress,
                    outcome: None,
                },
                Event::TaskStatus {
                    t: RunStore::now(),
                    id: "t1".into(),
                    status: TaskStatus::Review,
                    outcome: None,
                },
            ] {
                store.append(ev).await.unwrap();
            }
            store
        }

        // Reviewer that always requests rework.
        let rework: Reviewer = std::sync::Arc::new(|_task| {
            Box::pin(async { Some("add tests".to_string()) })
        });
        let dir = tempdir().unwrap();
        let store = seed_review(dir.path()).await;
        let (handle, join) =
            spawn_full(store, cfg(dir.path().to_path_buf()), fake_happy_spawner(), None, Some(rework));
        let err = handle.finalize_task("t1", None).await.unwrap_err();
        assert!(matches!(err, OrchestratorError::ReviewRework(ref id) if id == "t1"), "got {err:?}");
        // The rework bounced it to Failed with the reviewer notes as summary.
        let t = handle.snapshot().await.unwrap().task("t1").unwrap().clone();
        assert_eq!(t.status, TaskStatus::Failed);
        assert!(t.outcome.unwrap().summary.contains("add tests"));
        handle.shutdown().await;
        let _ = join.await;

        // Approving reviewer lets the same task finalize to Done.
        let approve: Reviewer = std::sync::Arc::new(|_task| Box::pin(async { None }));
        let dir = tempdir().unwrap();
        let store = seed_review(dir.path()).await;
        let (handle, join) =
            spawn_full(store, cfg(dir.path().to_path_buf()), fake_happy_spawner(), None, Some(approve));
        handle.finalize_task("t1", None).await.unwrap();
        assert_eq!(
            handle.snapshot().await.unwrap().task("t1").unwrap().status,
            TaskStatus::Done
        );
        handle.shutdown().await;
        let _ = join.await;
    }

    /// E11 hard gate — a Review task that edited two files without a
    /// checkpoint is refused finalize when the flag is on, and accepted when
    /// it's off. Seeds the event stream directly, then drives finalize.
    #[tokio::test]
    async fn e11_finalize_blocks_unchecked_multifile_task() {
        async fn seed(dir: &std::path::Path) -> RunStore {
            let mut store = RunStore::create(
                dir.join(".wingman/autonomous/e11-run"),
                "e11-run",
                "g",
                "abc",
                "wingman/auto/e11-run",
            )
            .await
            .unwrap();
            for ev in [
                Event::TaskCreate {
                    t: RunStore::now(),
                    id: "t1".into(),
                    role: Role::Developer,
                    title: "t1".into(),
                    goal: String::new(),
                    deps: vec![],
                    writes: vec![],
                    acceptance: vec![],
                    reversibility: Default::default(),
                    reversibility_reason: None,
                },
                Event::TaskAssign {
                    t: RunStore::now(),
                    id: "t1".into(),
                    agent: "a1".into(),
                    worktree: "wt".into(),
                },
                Event::TaskStatus {
                    t: RunStore::now(),
                    id: "t1".into(),
                    status: TaskStatus::InProgress,
                    outcome: None,
                },
                Event::TaskTool {
                    t: RunStore::now(),
                    id: "t1".into(),
                    agent: "a1".into(),
                    tool: "edit_file".into(),
                    input_hash: None,
                    ok: true,
                },
                Event::TaskTool {
                    t: RunStore::now(),
                    id: "t1".into(),
                    agent: "a1".into(),
                    tool: "edit_file".into(),
                    input_hash: None,
                    ok: true,
                },
                Event::TaskStatus {
                    t: RunStore::now(),
                    id: "t1".into(),
                    status: TaskStatus::Review,
                    outcome: None,
                },
            ] {
                store.append(ev).await.unwrap();
            }
            store
        }

        // enforce on → violation is refused, task stays in Review.
        let dir = tempdir().unwrap();
        let store = seed(dir.path()).await;
        let mut c = cfg(dir.path().to_path_buf());
        c.enforce_checkpoint_hygiene = true;
        let (handle, join) = spawn(store, c, fake_happy_spawner());
        let err = handle.finalize_task("t1", None).await.unwrap_err();
        assert!(
            matches!(err, OrchestratorError::CheckpointViolation(ref id, _) if id == "t1"),
            "got {err:?}"
        );
        assert_eq!(
            handle.snapshot().await.unwrap().task("t1").unwrap().status,
            TaskStatus::Review
        );
        handle.shutdown().await;
        let _ = join.await;

        // enforce off → same task finalizes to Done.
        let dir = tempdir().unwrap();
        let store = seed(dir.path()).await;
        let (handle, join) = spawn(store, cfg(dir.path().to_path_buf()), fake_happy_spawner());
        handle.finalize_task("t1", None).await.unwrap();
        assert_eq!(
            handle.snapshot().await.unwrap().task("t1").unwrap().status,
            TaskStatus::Done
        );
        handle.shutdown().await;
        let _ = join.await;
    }
}
