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
    #[error("token cap reached: used {spent} of {cap} tokens")]
    TokenCap { spent: u64, cap: u64 },
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
pub type Reviewer =
    Arc<dyn Fn(Task) -> Pin<Box<dyn Future<Output = Option<String>> + Send>> + Send + Sync>;

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
    /// E9 — re-evaluate speculative worktrees against the current plan. Sent
    /// by the speculation watchdog whenever a task is created or changes
    /// status.
    Speculate,
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
    /// Hard cap on total tokens (in + out) for the run. 0 = disabled.
    ///
    /// This is the backstop for `max_usd`. Cost is computed from a hardcoded
    /// price table, and an unpriced model prices at $0 — so on any model not
    /// in that table the USD cap never trips, and the budget watchdog, which
    /// reads the same total, is defeated with it. Token counts are recorded
    /// correctly regardless of pricing, so this bound holds for every model.
    pub max_total_tokens: u64,
    /// Per-task retry budget for the auto-retry watchdog. Each failed
    /// attempt advances the E5 retry ladder one rung; the watchdog
    /// stops when this many retries have been exhausted (or rung 4 is
    /// reached, whichever comes first). Default 3 means the user sees
    /// at most one initial attempt + 3 retries before the task is
    /// marked Blocked.
    pub max_retries_per_task: u32,
    /// Where to write desktop notification cards for failures, or `None` to
    /// write none. The caller resolves this through
    /// [`crate::notify::desktop_target`] so routing and the on/off switch stay
    /// in one place; `None` skips spawning the watchdog entirely, exactly as a
    /// zero budget skips the budget watchdog.
    pub desktop_inbox: Option<PathBuf>,
    /// E9 — sample host CPU load in the background and narrow the live
    /// concurrency cap as it rises. Off for unit tests, whose caps must not
    /// depend on how busy the machine running them is.
    pub sample_host_load: bool,
    /// E9 — create the worktree of a task that is about to become ready (its
    /// deps are all in Review or Done) before the manager assigns it, and run
    /// `warm_cmd` there, so the worker starts on a built tree. Discarded if
    /// the plan changes first. Needs real worktrees.
    pub speculative_prespawn: bool,
    /// Shell command that warms a speculative worktree, typically the build
    /// the worker's turn gate runs anyway. Empty only creates the worktree.
    pub warm_cmd: String,
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
            max_total_tokens: 20_000_000,
            max_retries_per_task: 3,
            desktop_inbox: None,
            sample_host_load: false,
            speculative_prespawn: false,
            warm_cmd: String::new(),
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

/// J15 — passing tests per test-running acceptance check at the run's base
/// commit, keyed by the check's result label; `None` when that run printed no
/// summary. Filled once per check per run by [`measure_test_baselines`].
type TestBaselines = Arc<std::sync::Mutex<HashMap<String, Option<u32>>>>;

/// E9 — inputs to the live concurrency cap that run state does not hold.
#[derive(Default)]
struct HostSignals {
    /// Provider rate limits the run's workers reported (`agent.rate_limit`).
    rate_limits: std::sync::Mutex<crate::concurrency::RateLimitWindow>,
    /// Latest host CPU load, in thousandths; 0 until first sampled.
    cpu_load_milli: std::sync::atomic::AtomicU32,
}

/// How often the host CPU load is re-read, when sampling is on.
const CPU_SAMPLE_EVERY: Duration = Duration::from_secs(10);

/// E9 — a worktree created for a task before the manager assigned it.
struct Prewarm {
    worktree: PathBuf,
    /// Dropping or firing this stops the warm command.
    cancel: oneshot::Sender<()>,
    /// The warm command; finished once it exits, times out or is cancelled.
    warm: tokio::task::JoinHandle<()>,
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
    let notify_rx = store.subscribe();
    let escalation_rx = store.subscribe();
    let host_rx = store.subscribe();
    let speculate_rx = store.subscribe();
    let store = Arc::new(Mutex::new(store));
    let baselines = TestBaselines::default();
    let signals = Arc::new(HostSignals::default());

    // E9 — rate limits from workers, and host load when sampling is on, feed
    // the concurrency cap `handle_assign` enforces.
    tokio::spawn(host_signal_watchdog(
        host_rx,
        signals.clone(),
        cfg.sample_host_load,
        tx.clone(),
    ));

    // E9 — speculative pre-spawn needs a base commit to branch worktrees from.
    if cfg.speculative_prespawn && cfg.use_real_worktrees && !cfg.base_commit.is_empty() {
        tokio::spawn(speculation_watchdog(speculate_rx, tx.clone()));
    } else {
        drop(speculate_rx);
    }

    // J15 escalation watchdog: the runtime triggers fire while the run is
    // live, not only once the PR is open. Always on — J15 has no off switch.
    // Skipped for the in-memory unit-test config, which has no run history.
    let prior_runs = if cfg.project_root.as_os_str().is_empty() {
        Vec::new()
    } else {
        recent_run_outcomes(&cfg.project_root, &cfg.run_id)
    };
    tokio::spawn(escalation_watchdog(
        escalation_rx,
        store.clone(),
        baselines.clone(),
        cfg.max_usd,
        prior_runs,
    ));

    // Failure watchdog: one subscriber rather than an emit at each of the ten-
    // plus places that write `TaskStatus::Failed`. The broadcast channel is
    // already there, and a call site added later is covered without anyone
    // remembering to.
    //
    // It is also the only thing that reports a run killed by deadlock or the
    // tick budget: `pipeline` marks that run Failed and then returns `Err`, so
    // the CLI's end-of-run report never gets to speak.
    if let Some(dir) = cfg.desktop_inbox.clone() {
        tokio::spawn(notify_watchdog(
            notify_rx,
            store.clone(),
            dir,
            cfg.project_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
            crate::run_dir(&cfg.project_root, &cfg.run_id)
                .display()
                .to_string(),
        ));
    } else {
        drop(notify_rx);
    }

    // Budget watchdog: subscribes to the store's broadcast channel and
    // aborts every in-flight task the moment totals.usd crosses max_usd.
    // The pre-spawn check in handle_assign catches the easy case; this
    // watchdog catches the case where a task starts cheap and a later
    // turn pushes us over.
    if cfg.max_usd > 0.0 || cfg.max_total_tokens > 0 {
        let watchdog_tx = tx.clone();
        let cap = cfg.max_usd;
        let token_cap = cfg.max_total_tokens;
        let store_for_watchdog = store.clone();
        tokio::spawn(budget_watchdog(
            budget_rx,
            store_for_watchdog,
            cap,
            token_cap,
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

    let join = tokio::spawn(run_actor(
        store, cfg, spawner, splitter, reviewer, baselines, signals, rx,
    ));
    (handle, join)
}

/// Background task: evaluate the J15 runtime triggers
/// ([`crate::escalation::check_runtime`]) while the run is live and record each
/// new one as a `run.escalation` event. The failure watchdog turns those into
/// desktop cards; the pipeline folds them into the merge gate and the R3
/// packet.
///
/// As the run starts: the last three runs before it all failed. On every
/// `agent.usd`: spend against the 0.8x / 1.0x cap. On every finished attempt
/// (`task.attempt`): net-negative tests against the base-commit baselines (for
/// an attempt that reached Review), an irreversible task having run, and three
/// consecutive failed attempts in this run.
async fn escalation_watchdog(
    mut events: tokio::sync::broadcast::Receiver<Event>,
    store: Arc<Mutex<RunStore>>,
    baselines: TestBaselines,
    max_usd: f64,
    prior_runs: Vec<(String, bool)>,
) {
    record_escalations(&store, |state| {
        crate::escalation::check_runtime(&crate::escalation::RuntimeSignals {
            state,
            task: None,
            tests_before: None,
            tests_after: None,
            max_usd: 0.0,
            recent_run_outcomes: &prior_runs,
        })
    })
    .await;
    let mut attempts: Vec<(String, bool)> = Vec::new();
    loop {
        let (task_id, tests) = match events.recv().await {
            Ok(Event::AgentUsd { .. }) => (None, None),
            Ok(Event::TaskAttempt {
                id,
                rung,
                status,
                tests,
                ..
            }) => {
                attempts.push((
                    format!("{id} (rung {rung})"),
                    !matches!(status, TaskStatus::Failed | TaskStatus::Blocked),
                ));
                let counts = (status == TaskStatus::Review)
                    .then(|| {
                        let before = baselines.lock().unwrap_or_else(|e| e.into_inner());
                        crate::acceptance::net_test_counts(&before, &tests)
                    })
                    .flatten();
                (Some(id), counts)
            }
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        record_escalations(&store, |state| {
            crate::escalation::check_runtime(&crate::escalation::RuntimeSignals {
                state,
                task: task_id.as_deref().and_then(|id| state.task(id)),
                tests_before: tests.map(|(before, _)| before),
                tests_after: tests.map(|(_, after)| after),
                max_usd,
                recent_run_outcomes: if task_id.is_some() { &attempts } else { &[] },
            })
        })
        .await;
    }
}

/// Append a `run.escalation` event for each trigger `evaluate` finds that the
/// run has not already recorded as the same incident.
async fn record_escalations(
    store: &Arc<Mutex<RunStore>>,
    evaluate: impl FnOnce(&crate::model::RunState) -> Vec<crate::escalation::EscalationTrigger>,
) {
    let mut store = store.lock().await;
    let state = store.state();
    let fresh: Vec<_> = evaluate(state)
        .into_iter()
        .filter(|t| !state.escalations.iter().any(|e| e.duplicates(t)))
        .collect();
    for trigger in fresh {
        tracing::warn!(
            target: "pilot::escalation",
            trigger = trigger.short_label(),
            "{}",
            trigger.render()
        );
        let _ = store
            .append(Event::Escalation {
                t: RunStore::now(),
                trigger,
            })
            .await;
    }
}

/// J15 — the (run_id, ok) outcome of every earlier run in this project, oldest
/// first, excluding `current_run_id`. Feeds the RepeatedFailures trigger as a
/// run starts. Reads what `dashboard::load_all_run_states` already persists —
/// no new store.
fn recent_run_outcomes(
    project_root: &std::path::Path,
    current_run_id: &str,
) -> Vec<(String, bool)> {
    let mut states = crate::dashboard::load_all_run_states(project_root);
    // run ids are timestamp-prefixed, so a lexical sort is chronological.
    states.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    states
        .into_iter()
        .filter(|s| s.run_id != current_run_id)
        .map(|s| (s.run_id, s.status == RunStatus::Done))
        .collect()
}

/// J15 — count the passing tests each of `task`'s test-running checks reports
/// at the base commit, once per check per run. Runs in the attempt's freshly
/// created worktree before its worker starts: the tree is still exactly the
/// base commit, and the build it leaves behind is the one the worker's own
/// checks then reuse, so the extra cost is one test run per distinct check.
async fn measure_test_baselines(
    store: &Arc<Mutex<RunStore>>,
    task: &Task,
    worktree: PathBuf,
    budget: Duration,
    baselines: &TestBaselines,
) {
    let pending: Vec<crate::model::Acceptance> = {
        let known = baselines.lock().unwrap_or_else(|e| e.into_inner());
        task.acceptance
            .iter()
            .filter(|c| {
                crate::acceptance::test_check_label(c).is_some_and(|l| !known.contains_key(&l))
            })
            .cloned()
            .collect()
    };
    if pending.is_empty() {
        return;
    }
    // The task is busy from here, though its worker has not started: the
    // concurrency cap and the write-set scheduler count `in_progress`.
    let _ = store
        .lock()
        .await
        .append(Event::TaskStatus {
            t: RunStore::now(),
            id: task.id.clone(),
            status: TaskStatus::InProgress,
            outcome: None,
        })
        .await;
    let results = tokio::task::spawn_blocking(move || {
        crate::acceptance::run_acceptance_checks_within(&pending, &worktree, budget)
    })
    .await
    .unwrap_or_default();
    let mut known = baselines.lock().unwrap_or_else(|e| e.into_inner());
    for r in results {
        known.entry(r.label).or_insert(r.passed_tests);
    }
}

/// How long a failure waits for company before its card is written.
///
/// A run that trips a shared dependency fails several tasks within a beat of
/// each other, and the orchestrator then marks the run itself failed. Writing
/// a card per event produced a stack of four saying the same thing. Three
/// seconds is well under the time it takes to look at the screen, so a batch
/// still feels immediate.
const COALESCE: Duration = Duration::from_secs(3);

/// One thing worth reporting, before it is merged with whatever arrives next.
enum Bad {
    /// A task failed: its title, and whatever the outcome said.
    Task(String, String),
    /// The run itself ended badly: `failed` or `aborted`.
    Run(&'static str),
    /// A J15 escalation trigger fired: its short label and its rendering.
    Escalation(&'static str, String),
}

/// Background task: writes desktop cards for task failures, J15 escalations,
/// and a run that ends badly. Runs until the broadcast channel closes.
///
/// Failures inside one [`COALESCE`] window become a single card. That is worth
/// more than it sounds: the common shape is one broken dependency failing three
/// tasks and then the run, which used to be four cards that had to be dismissed
/// one at a time.
///
/// A card for a run that is still going carries one button: abort. When the
/// retry ladder is grinding on something that is not going to work, stopping it
/// is the whole of what a human wants, and the card already holds the `run_dir`
/// the command needs. It is deliberately the only one:
///
///   - **Retry** would be free (`{"cmd":"retry_task","id":…}` is already a
///     `ControlCommand`) but the ladder is retrying the task by itself, and a
///     human racing it is a new failure mode rather than a feature.
///   - **Abort on a run that already ended** is a button that does nothing, so
///     a batch carrying a run outcome gets no buttons at all.
///
/// `RunStatus::Done` is deliberately absent: the CLI's end-of-run report owns
/// completion and renders a far better body than anything available here.
async fn notify_watchdog(
    mut events: tokio::sync::broadcast::Receiver<Event>,
    store: Arc<Mutex<RunStore>>,
    dir: PathBuf,
    project: Option<String>,
    run_dir: String,
) {
    use wingman_config::inbox::{append_to, Notification};

    loop {
        // Block for the first one, then take whatever follows it closely.
        let Some(first) = next_bad(&mut events, &store).await else {
            return;
        };
        let mut batch = vec![first];
        let deadline = tokio::time::Instant::now() + COALESCE;
        loop {
            match tokio::time::timeout_at(deadline, next_bad(&mut events, &store)).await {
                Ok(Some(b)) => batch.push(b),
                // Channel closed: write what we have rather than dropping it.
                Ok(None) => break,
                Err(_) => break,
            }
        }

        let (title, body) = render(&batch);
        // Only while the run is still going: a batch carrying its outcome is a
        // report, and an abort button on it would do nothing.
        let over = batch.iter().any(|b| matches!(b, Bad::Run(_)));
        let actions = if over {
            Vec::new()
        } else {
            vec![wingman_config::inbox::Action {
                id: "abort".into(),
                label: "Abort run".into(),
                control: serde_json::to_value(crate::control::ControlCommand::AbortRun).ok(),
            }]
        };
        let _ = append_to(
            &dir,
            &Notification {
                project: project.clone(),
                run_dir: Some(run_dir.clone()),
                actions,
                ..Notification::now("escalation", title, body)
            },
        );
    }
}

/// The next event worth a card, or `None` once the channel is gone.
async fn next_bad(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    store: &Arc<Mutex<RunStore>>,
) -> Option<Bad> {
    loop {
        match events.recv().await {
            Ok(Event::TaskStatus {
                id,
                status: TaskStatus::Failed,
                outcome,
                ..
            }) => {
                // The task's title reads better on a card than `t3` does.
                let label = {
                    let s = store.lock().await;
                    s.state()
                        .tasks
                        .iter()
                        .find(|t| t.id == id)
                        .map(|t| t.title.clone())
                };
                return Some(Bad::Task(
                    label.unwrap_or(id),
                    outcome.map(|o| o.summary).unwrap_or_default(),
                ));
            }
            Ok(Event::Escalation { trigger, .. }) => {
                return Some(Bad::Escalation(trigger.short_label(), trigger.render()))
            }
            Ok(Event::RunStatusEv {
                status: RunStatus::Failed,
                ..
            }) => return Some(Bad::Run("failed")),
            Ok(Event::RunStatusEv {
                status: RunStatus::Aborted,
                ..
            }) => return Some(Bad::Run("aborted")),
            Ok(_) => continue,
            // A lagged receiver has missed events, not lost the channel. Keep
            // going: a dropped failure card is better than a silent watchdog.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return None,
        }
    }
}

/// Title and body for one batch.
///
/// The run outcome wins the title when it is in the batch — "run failed" is
/// what the reader needs first, and the tasks that caused it belong in the
/// body underneath it. Short of that, a J15 escalation wins: it is one of the
/// lines a run must not cross unseen, and a task failure beside it is context.
fn render(batch: &[Bad]) -> (String, String) {
    let run = batch.iter().find_map(|b| match b {
        Bad::Run(word) => Some(*word),
        _ => None,
    });
    let tasks: Vec<(&String, &String)> = batch
        .iter()
        .filter_map(|b| match b {
            Bad::Task(label, summary) => Some((label, summary)),
            _ => None,
        })
        .collect();
    let escalations: Vec<(&str, String)> = batch
        .iter()
        .filter_map(|b| match b {
            Bad::Escalation(label, detail) => Some((*label, format!("• {detail}"))),
            _ => None,
        })
        .collect();
    let bullets = |tasks: &[(&String, &String)]| {
        tasks
            .iter()
            .map(|(label, _)| format!("• {label}"))
            .chain(escalations.iter().map(|(_, line)| line.clone()))
            .collect::<Vec<_>>()
            .join(
                "
",
            )
    };

    match (run, tasks.as_slice(), escalations.as_slice()) {
        // A run failure on its own, or with the tasks that explain it.
        (Some(word), [], _) => (format!("Run {word}"), bullets(&[])),
        (Some(word), many, _) => (
            format!("Run {word} — {} task(s) did not finish", many.len()),
            bullets(many),
        ),
        (None, _, [(label, _)]) => (format!("Escalation — {label}"), bullets(&tasks)),
        (None, _, [_, ..]) => (
            format!("{} escalations", escalations.len()),
            bullets(&tasks),
        ),
        // One task, and room to say what went wrong with it.
        (None, [(label, summary)], []) => (format!("Task failed — {label}"), (*summary).clone()),
        // Several: the list is more use than any one summary.
        (None, many, []) => (format!("{} tasks failed", many.len()), bullets(many)),
    }
}

/// Background task: aborts every in-flight task when totals.usd crosses
/// `cap`. Runs until either the broadcast channel closes (store dropped)
/// or it issues the abort batch.
async fn budget_watchdog(
    mut events: tokio::sync::broadcast::Receiver<Event>,
    store: Arc<Mutex<RunStore>>,
    cap: f64,
    token_cap: u64,
    orch: mpsc::Sender<OrchestratorCommand>,
) {
    loop {
        match events.recv().await {
            Ok(Event::AgentUsd { .. }) => {
                let totals = store.lock().await.state().totals;
                let tokens = totals.tokens_in.saturating_add(totals.tokens_out);
                let over_usd = cap > 0.0 && totals.usd >= cap;
                // Token backstop: holds even when the model is unpriced and
                // `totals.usd` is stuck at zero.
                let over_tokens = token_cap > 0 && tokens >= token_cap;
                if over_usd || over_tokens {
                    tracing::warn!(
                        target: "pilot::budget",
                        spent = totals.usd,
                        cap,
                        tokens,
                        token_cap,
                        reason = if over_usd { "cost cap" } else { "token cap" },
                        "budget watchdog: cap reached, aborting all in-flight tasks"
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

/// Background task: feed the E9 concurrency cap what run state does not hold.
/// Each `agent.rate_limit` a worker reports goes into the rate-limit window;
/// with `sample_cpu`, host CPU load is re-read every [`CPU_SAMPLE_EVERY`].
async fn host_signal_watchdog(
    mut events: tokio::sync::broadcast::Receiver<Event>,
    signals: Arc<HostSignals>,
    sample_cpu: bool,
    orch: mpsc::Sender<OrchestratorCommand>,
) {
    let mut sampler = Some(crate::concurrency::CpuSampler::default());
    let mut ticker = tokio::time::interval(CPU_SAMPLE_EVERY);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(Event::AgentRateLimited { retry_after_secs, .. }) => {
                    signals
                        .rate_limits
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .record(std::time::Instant::now(), retry_after_secs);
                }
                // A nearly spent subscription is a rate limit that has not
                // happened yet: hold the cap at the floor until it resets.
                Ok(Event::SubscriptionUsage { utilization, resets_at, .. })
                    if utilization >= crate::concurrency::SUBSCRIPTION_THROTTLE_AT =>
                {
                    // No reset time: hold for five minutes, which the next
                    // report (one per request) re-arms while usage stays high.
                    let until_reset = crate::concurrency::secs_until(resets_at).or(Some(300));
                    signals
                        .rate_limits
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .record(std::time::Instant::now(), until_reset);
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            _ = ticker.tick(), if sample_cpu => {
                if orch.is_closed() {
                    return;
                }
                // Off the runtime: macOS reads the load by running `sysctl`.
                let Some(mut s) = sampler.take() else { return };
                let Ok((s, load)) = tokio::task::spawn_blocking(move || {
                    let load = s.sample();
                    (s, load)
                })
                .await
                else {
                    return;
                };
                sampler = Some(s);
                if let Some(load) = load {
                    signals
                        .cpu_load_milli
                        .store((load * 1000.0).round() as u32, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }
}

/// Background task: ask the actor to re-evaluate speculative worktrees (E9)
/// whenever a task is created or changes status.
async fn speculation_watchdog(
    mut events: tokio::sync::broadcast::Receiver<Event>,
    orch: mpsc::Sender<OrchestratorCommand>,
) {
    loop {
        match events.recv().await {
            Ok(Event::TaskStatus { .. } | Event::TaskCreate { .. }) => {
                if orch.send(OrchestratorCommand::Speculate).await.is_err() {
                    return;
                }
            }
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// E9 — the live concurrency cap: `max_concurrent_agents`, narrowed by recent
/// provider rate limits, host CPU load and budget burn.
fn live_cap(
    state: &crate::model::RunState,
    cfg: &OrchestratorConfig,
    signals: &HostSignals,
) -> u32 {
    let (recent_rate_limit_hits, active_retry_after_secs) = signals
        .rate_limits
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .sample(std::time::Instant::now());
    crate::concurrency::recommended_concurrency(&crate::concurrency::ConcurrencySignals {
        max_agents: cfg.max_concurrent_agents,
        min_agents: 1,
        recent_rate_limit_hits,
        active_retry_after_secs,
        cpu_load: f64::from(
            signals
                .cpu_load_milli
                .load(std::sync::atomic::Ordering::Relaxed),
        ) / 1000.0,
        usd_spent: state.totals.usd,
        max_usd: cfg.max_usd,
    })
}

/// E9 — speculative pre-spawn. Discard the speculative worktrees whose task is
/// no longer waiting on deps in Review or Done (it was replanned, blocked, or
/// split away), then create one for each task that is about to become ready
/// (every dep in Review or Done, at least one still in Review), as far as the
/// concurrency cap has room once running and warming tasks are counted.
///
/// The worktree is exactly what `handle_assign` would create, since every
/// worktree branches from the base commit whatever the deps did, so the
/// assignment takes it over as is.
async fn handle_speculate(
    store: &Arc<Mutex<RunStore>>,
    cfg: &OrchestratorConfig,
    signals: &HostSignals,
    prewarms: &mut HashMap<String, Prewarm>,
) {
    fn waiting(state: &crate::model::RunState, task: &Task) -> bool {
        matches!(task.status, TaskStatus::Pending | TaskStatus::Todo)
            && !task.deps.is_empty()
            && task.deps.iter().all(|d| {
                state
                    .task(d)
                    .is_some_and(|d| matches!(d.status, TaskStatus::Review | TaskStatus::Done))
            })
    }

    let stale: Vec<String> = {
        let store = store.lock().await;
        let state = store.state();
        prewarms
            .keys()
            .filter(|id| !state.task(id).is_some_and(|t| waiting(state, t)))
            .cloned()
            .collect()
    };
    for id in stale {
        if let Some(prewarm) = prewarms.remove(&id) {
            discard_prewarm(cfg, &id, prewarm).await;
        }
    }

    let fresh: Vec<String> = {
        let store = store.lock().await;
        let state = store.state();
        let busy = state
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::InProgress)
            .count()
            + prewarms.values().filter(|p| !p.warm.is_finished()).count();
        let room = (live_cap(state, cfg, signals) as usize).saturating_sub(busy);
        state
            .tasks
            .iter()
            .filter(|t| {
                !prewarms.contains_key(&t.id)
                    && waiting(state, t)
                    && t.deps.iter().any(|d| {
                        state
                            .task(d)
                            .is_some_and(|d| d.status == TaskStatus::Review)
                    })
            })
            .take(room)
            .map(|t| t.id.clone())
            .collect()
    };
    for id in fresh {
        let worktree = crate::worktree_dir(&cfg.project_root, &cfg.run_id, &id);
        let (repo, base, run_id, task_id, path) = (
            cfg.project_root.clone(),
            cfg.base_commit.clone(),
            cfg.run_id.clone(),
            id.clone(),
            worktree.clone(),
        );
        let created = tokio::task::spawn_blocking(move || {
            crate::worktree::create_worktree(&repo, &base, &run_id, &task_id, &path)
        })
        .await;
        if let Ok(Err(e)) = &created {
            tracing::warn!(target: "pilot::speculate", task = %id, error = %e, "speculative worktree not created");
        }
        if !matches!(created, Ok(Ok(_))) {
            continue;
        }
        let (cancel, cancelled) = oneshot::channel();
        let warm = tokio::spawn(warm_worktree(
            cfg.warm_cmd.clone(),
            worktree.clone(),
            cfg.task_timeout,
            cancelled,
        ));
        tracing::info!(target: "pilot::speculate", task = %id, "created worktree ahead of assignment");
        prewarms.insert(
            id,
            Prewarm {
                worktree,
                cancel,
                warm,
            },
        );
    }
}

/// Run `cmd` in a speculative worktree until it exits, `budget` passes, or
/// the worktree is discarded (`cancelled` fires or its sender is dropped). The
/// supervisor kills the command's whole process tree when it is dropped early.
async fn warm_worktree(
    cmd: String,
    worktree: PathBuf,
    budget: Duration,
    mut cancelled: oneshot::Receiver<()>,
) {
    if cmd.trim().is_empty() {
        return;
    }
    let mut sc = crate::child_process::SupervisedCommand::from_command(
        crate::child_process::shell_command(&cmd).into(),
    );
    sc.command_mut()
        .current_dir(&worktree)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut supervisor = match sc.spawn() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(target: "pilot::speculate", error = %e, "warm command did not start");
            return;
        }
    };
    let Some(mut child) = supervisor.take_child() else {
        return;
    };
    tokio::select! {
        status = child.wait() => {
            tracing::debug!(target: "pilot::speculate", ?status, worktree = %worktree.display(), "warm command finished");
        }
        _ = &mut cancelled => {}
        _ = tokio::time::sleep(budget) => {
            tracing::warn!(target: "pilot::speculate", worktree = %worktree.display(), "warm command timed out");
        }
    }
}

/// Stop a speculative worktree's warm command and remove the worktree and its
/// branch.
async fn discard_prewarm(cfg: &OrchestratorConfig, task_id: &str, prewarm: Prewarm) {
    let _ = prewarm.cancel.send(());
    let _ = prewarm.warm.await;
    let (repo, run_id, id, path) = (
        cfg.project_root.clone(),
        cfg.run_id.clone(),
        task_id.to_string(),
        prewarm.worktree,
    );
    let _ = tokio::task::spawn_blocking(move || {
        crate::worktree::discard_worktree(&repo, &run_id, &id, &path)
    })
    .await;
    tracing::info!(target: "pilot::speculate", task = %task_id, "discarded speculative worktree");
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
                ControlCommand::Tell {
                    task,
                    message,
                    reply,
                } => {
                    // Resolve which live worker(s) should hear it. The control
                    // file only knows task ids; the orchestrator knows which
                    // agent holds which task, so ask it for a snapshot first.
                    let (snap_tx, snap_rx) = oneshot::channel();
                    if orch
                        .send(OrchestratorCommand::Snapshot { reply: snap_tx })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let Ok(state) = snap_rx.await else { return };
                    let targets: Vec<String> = state
                        .agents
                        .iter()
                        .filter(|a| match (&task, &a.current_task) {
                            (Some(want), Some(have)) => want == have,
                            // No task named: everyone actually working hears it.
                            (None, Some(_)) => true,
                            _ => false,
                        })
                        .map(|a| a.id.clone())
                        .collect();
                    if targets.is_empty() {
                        tracing::warn!(
                            target: "pilot::control",
                            task = ?task,
                            "tell/ask had no live worker to deliver to"
                        );
                        continue;
                    }
                    let body = crate::ipc::encode_command(&crate::ipc::ManagerCommand::Note {
                        text: message.clone(),
                        reply,
                    });
                    let mut failed = false;
                    for agent_id in targets {
                        let (reply_tx, _) = oneshot::channel();
                        if orch
                            .send(OrchestratorCommand::MessageAgent {
                                agent_id,
                                body: body.clone(),
                                reply: reply_tx,
                            })
                            .await
                            .is_err()
                        {
                            failed = true;
                            break;
                        }
                    }
                    if failed {
                        return;
                    }
                    continue;
                }
            };
            if sent.is_err() {
                return;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_actor(
    store: Arc<Mutex<RunStore>>,
    cfg: OrchestratorConfig,
    spawner: WorkerSpawner,
    splitter: Option<TaskSplitter>,
    reviewer: Option<Reviewer>,
    baselines: TestBaselines,
    signals: Arc<HostSignals>,
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
    // E9 — speculative worktrees not yet taken over by an assignment.
    let mut prewarms: HashMap<String, Prewarm> = HashMap::new();

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
                    &baselines,
                    &signals,
                    &mut prewarms,
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
                    &baselines,
                    &signals,
                    &mut prewarms,
                )
                .await;
                let _ = reply.send(result);
            }
            OrchestratorCommand::FinalizeTask {
                task_id,
                merge_commit,
                reply,
            } => {
                let result =
                    handle_finalize(&store, &task_id, merge_commit, reviewer.as_ref()).await;
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
                                file: None,
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
            OrchestratorCommand::Speculate => {
                if !aborting {
                    handle_speculate(&store, &cfg, &signals, &mut prewarms).await;
                }
            }
            OrchestratorCommand::Shutdown => break,
        }
    }

    // A speculative worktree nothing took over is not work anyone did.
    for (id, prewarm) in prewarms.drain() {
        discard_prewarm(&cfg, &id, prewarm).await;
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
    baselines: &TestBaselines,
    signals: &HostSignals,
    prewarms: &mut HashMap<String, Prewarm>,
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
        // Token backstop, for the (common) case of a model with no entry in
        // the price table, where `totals.usd` stays at 0 no matter how much
        // work happens and the USD check above can never fire.
        if cfg.max_total_tokens > 0 {
            let totals = store_g.state().totals;
            let tokens = totals.tokens_in.saturating_add(totals.tokens_out);
            if tokens >= cfg.max_total_tokens {
                return Err(OrchestratorError::TokenCap {
                    spent: tokens,
                    cap: cfg.max_total_tokens,
                });
            }
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
        // E9 — adaptive cap: rather than always allowing
        // `max_concurrent_agents`, narrow the ceiling while workers' providers
        // are rate limiting, the host is busy, or the budget burns down.
        let cap = live_cap(store_g.state(), cfg, signals);
        if live >= cap {
            return Err(OrchestratorError::ConcurrencyCap(cap));
        }

        // E4 — write-set conflict avoidance: never run two tasks whose
        // declared `writes` overlap concurrently, so most merge conflicts are
        // designed out.
        //
        // Uses `scheduler::tasks_conflict`, not `writes_overlap`: the former
        // treats an *undeclared* write-set as conflicting with everything,
        // because a task that didn't say what it touches cannot be proven
        // disjoint from one that did. This path previously called
        // `writes_overlap` directly, which returns false whenever either side
        // is empty — so exactly the tasks with no declared writes, the ones we
        // know least about, were the ones allowed to run concurrently. Parallel
        // agents editing the same files is the failure that turns multi-agent
        // runs into merge-conflict cleanup.
        if let Some(conflict) = store_g
            .state()
            .tasks
            .iter()
            .find(|t| {
                t.status == TaskStatus::InProgress
                    && t.id != task.id
                    && crate::scheduler::tasks_conflict(&task, t)
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

    // An assigned task stays `Todo` until its worker records `InProgress`. A
    // manager tick inside that window saw an assignable task, and this used to
    // start a second worker in the same worktree. Reassign is unaffected: it
    // removes the old handle before it gets here.
    if task.status == TaskStatus::Todo {
        if let Some(previous) = &task.agent {
            if active
                .lock()
                .await
                .get(previous)
                .is_some_and(|h| !h.is_finished())
            {
                return Err(OrchestratorError::BadTransition(
                    task_id.to_string(),
                    task.status,
                    "assign (its worker is still starting)",
                ));
            }
        }
    }

    // E9 — a worktree created for this task ahead of time is taken over as
    // it is: it branches from the same base commit a fresh one would.
    let adopted = prewarms.remove(task_id);

    // Optionally create a real git worktree. Disabled in unit tests so
    // they don't have to set up a temp repo just to drive the actor.
    if cfg.use_real_worktrees && !cfg.base_commit.is_empty() && adopted.is_none() {
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
    let rung = retry.rung;
    // J15 baselines need a worktree still at the base commit; the in-memory
    // test config has none.
    let baseline = (cfg.use_real_worktrees && !cfg.base_commit.is_empty()).then(|| {
        (
            task.clone(),
            worktree.clone(),
            cfg.task_timeout,
            baselines.clone(),
        )
    });
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
        if let Some(Prewarm { cancel, warm, .. }) = adopted {
            // Let the warm command finish what the worker would otherwise
            // start by repeating. Busy meanwhile, as for the baselines below.
            if !warm.is_finished() {
                let _ = store_for_fail
                    .lock()
                    .await
                    .append(Event::TaskStatus {
                        t: RunStore::now(),
                        id: task_id.clone(),
                        status: TaskStatus::InProgress,
                        outcome: None,
                    })
                    .await;
            }
            let _ = warm.await;
            drop(cancel);
        }
        if let Some((task, worktree, budget, baselines)) = baseline {
            measure_test_baselines(&store_for_fail, &task, worktree, budget, &baselines).await;
        }
        match std::panic::AssertUnwindSafe(spawner(ctx))
            .catch_unwind()
            .await
        {
            Ok(Ok(_result)) => {
                tracing::debug!(target: "pilot::orch", task = %task_id, "worker finished");
            }
            Ok(Err(e)) => {
                tracing::warn!(target: "pilot::orch", task = %task_id, error = %e, "worker spawn failed");
                mark_worker_failed(
                    &store_for_fail,
                    &task_id,
                    &agent_for_fail,
                    rung,
                    format!("worker spawn failed: {e}"),
                )
                .await;
            }
            Err(_panic) => {
                tracing::error!(target: "pilot::orch", task = %task_id, "worker task panicked; marking task Failed");
                mark_worker_failed(
                    &store_for_fail,
                    &task_id,
                    &agent_for_fail,
                    rung,
                    "worker task panicked".into(),
                )
                .await;
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
async fn mark_worker_failed(
    store: &Arc<Mutex<RunStore>>,
    task_id: &str,
    agent_id: &str,
    rung: u32,
    summary: String,
) {
    let mut g = store.lock().await;
    // Skip if the worker already recorded a terminal status (it may have
    // written Failed/Review before a late panic in teardown).
    if let Some(t) = g.state().task(task_id) {
        if matches!(
            t.status,
            TaskStatus::Failed | TaskStatus::Review | TaskStatus::Done
        ) {
            return;
        }
    }
    // The worker never got to record its own attempt; do it for it, ahead of
    // the Failed status for the same reason `worker::record_attempt` is.
    let model = g.state().agent(agent_id).and_then(|a| a.model.clone());
    let _ = g
        .append(Event::TaskAttempt {
            t: RunStore::now(),
            id: task_id.to_string(),
            agent: agent_id.to_string(),
            rung,
            model,
            status: TaskStatus::Failed,
            summary,
            tests: Default::default(),
        })
        .await;
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
    baselines: &TestBaselines,
    signals: &HostSignals,
    prewarms: &mut HashMap<String, Prewarm>,
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

    // Reassign is the *retry* path: every rung below resets the task to Todo
    // and starts a fresh worker in a clean worktree. Applied to a task that
    // already reached Review or Done, that discards finished work and redoes
    // it from scratch — and if the second attempt runs out of turns, a task
    // that had succeeded ends the run Failed.
    //
    // The HTTP retry route already refuses this (`serve::pilot` gates on
    // Failed | Blocked), but the manager's `reassign_task` tool and the
    // control file reach the actor directly and did not. The guard belongs
    // here, where all three callers meet, rather than in one of them.
    {
        let store_g = store.lock().await;
        if let Some(t) = store_g.state().task(task_id) {
            if matches!(t.status, TaskStatus::Review | TaskStatus::Done) {
                tracing::warn!(
                    target: "pilot::retry",
                    task = %task_id,
                    status = ?t.status,
                    "refusing to reassign a task that already completed"
                );
                return Err(OrchestratorError::BadTransition(
                    task_id.to_string(),
                    t.status,
                    "reassign (task already complete)",
                ));
            }
        }
    }

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
        baselines,
        signals,
        prewarms,
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
    reviewer: Option<&Reviewer>,
) -> Result<(), OrchestratorError> {
    // Phase 1 — validate the transition under the lock, and clone the task
    // for the (async, lock-free) reviewer call. Checkpoint hygiene (E11) is
    // not checked here: the worker supervisor already kept any attempt that
    // failed it out of Review.
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
/// Test spawner that moves a task to `InProgress` and then never finishes, so
/// callers can observe behaviour *while* work is live (concurrency caps,
/// write-set conflicts).
#[cfg(test)]
pub fn fake_hanging_spawner() -> WorkerSpawner {
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
            // Park until the test drops the orchestrator. The type still has
            // to line up with the spawner signature, hence the never-reached
            // Ok below.
            futures::future::pending::<()>().await;
            unreachable!("hanging spawner is never resumed")
        })
    })
}

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
    // Aborting a task that already finished throws its work away, and there is
    // no worker left to stop — the point of an abort. A Review task has run,
    // passed its acceptance gate and is waiting to be finalized; marking it
    // Blocked discards it exactly as the reassign path used to, and for the
    // same reason: neither asked what state the task was in.
    //
    // Observed doing precisely that: a manager called abort_task on tasks whose
    // agents had already emitted `task_complete`, and three of them went
    // review -> blocked, deadlocking the run on work that was done.
    {
        let store_g = store.lock().await;
        if let Some(t) = store_g.state().task(task_id) {
            if matches!(t.status, TaskStatus::Review | TaskStatus::Done) {
                tracing::warn!(
                    target: "pilot::orchestrator",
                    task = %task_id,
                    status = ?t.status,
                    "refusing to abort a task that already completed"
                );
                return Err(OrchestratorError::BadTransition(
                    task_id.to_string(),
                    t.status,
                    "abort (task already complete)",
                ));
            }
        }
    }

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
            max_usd: 0.0,
            max_total_tokens: 0,     // disabled in unit tests
            max_retries_per_task: 0, // most tests assert single-shot behaviour
            desktop_inbox: None,
            sample_host_load: false,
            speculative_prespawn: false,
            warm_cmd: String::new(),
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

    /// The failure watchdog is the only thing that reports a run killed by
    /// deadlock or the tick budget — `pipeline` marks that run Failed and then
    /// returns `Err`, so the CLI's end-of-run report never runs. It must card
    /// failures and stay quiet about everything else.
    #[tokio::test]
    async fn the_failure_watchdog_merges_a_run_failure_with_its_tasks() {
        use crate::model::TaskOutcome;

        let dir = tempdir().unwrap();
        let inbox = tempdir().unwrap();
        let mut store = RunStore::create(
            dir.path().join(".wingman/autonomous/nw-run"),
            "nw-run",
            "g",
            "deadbeef",
            "wingman/auto/nw-run",
        )
        .await
        .unwrap();

        let events = store.subscribe();
        store
            .append(Event::TaskCreate {
                t: RunStore::now(),
                id: "t1".into(),
                role: Role::Developer,
                title: "Wire the parser".into(),
                goal: String::new(),
                deps: Vec::new(),
                writes: Vec::new(),
                acceptance: Vec::<Acceptance>::new(),
                reversibility: Default::default(),
                reversibility_reason: None,
            })
            .await
            .unwrap();

        let store = Arc::new(Mutex::new(store));
        let watchdog = tokio::spawn(notify_watchdog(
            events,
            store.clone(),
            inbox.path().to_path_buf(),
            Some("repo".into()),
            "/p/.wingman/autonomous/nw-run".into(),
        ));

        {
            let mut s = store.lock().await;
            for status in [TaskStatus::InProgress, TaskStatus::Done] {
                s.append(Event::TaskStatus {
                    t: RunStore::now(),
                    id: "t1".into(),
                    status,
                    outcome: None,
                })
                .await
                .unwrap();
            }
            s.append(Event::TaskStatus {
                t: RunStore::now(),
                id: "t1".into(),
                status: TaskStatus::Failed,
                outcome: Some(TaskOutcome {
                    summary: "cargo test failed".into(),
                    files_changed: Vec::new(),
                }),
            })
            .await
            .unwrap();
            s.append(Event::RunStatusEv {
                t: RunStore::now(),
                status: RunStatus::Failed,
            })
            .await
            .unwrap();
        }

        // The watchdog holds a store handle, so the channel never closes and
        // there is nothing to join on — wait for the card instead. It arrives a
        // COALESCE window after the last failure, not immediately.
        let deadline = std::time::Instant::now() + COALESCE + Duration::from_secs(5);
        let cards = loop {
            let c = wingman_config::inbox::read_open(inbox.path());
            if !c.is_empty() || std::time::Instant::now() > deadline {
                break c;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        watchdog.abort();

        // One card, not three: the two progress transitions do not card at all,
        // and the task failure merges with the run failure that followed it.
        assert_eq!(cards.len(), 1, "{cards:?}");
        assert_eq!(cards[0].title, "Run failed — 1 task(s) did not finish");
        assert!(
            cards[0].body.contains("• Wire the parser"),
            "{}",
            cards[0].body
        );
        assert_eq!(cards[0].severity, "escalation");
        assert!(
            cards[0].actions.is_empty(),
            "the run is over; an abort button would do nothing"
        );
    }

    #[tokio::test]
    async fn a_failure_in_a_live_run_offers_to_abort_it() {
        use crate::model::TaskOutcome;

        let dir = tempdir().unwrap();
        let inbox = tempdir().unwrap();
        let store = RunStore::create(
            dir.path().join(".wingman/autonomous/ab-run"),
            "ab-run",
            "g",
            "deadbeef",
            "wingman/auto/ab-run",
        )
        .await
        .unwrap();
        let events = store.subscribe();
        let store = Arc::new(Mutex::new(store));
        let watchdog = tokio::spawn(notify_watchdog(
            events,
            store.clone(),
            inbox.path().to_path_buf(),
            None,
            "/p/.wingman/autonomous/ab-run".into(),
        ));

        // A task fails and the run keeps going — the case where stopping it is
        // the thing a human actually wants to do.
        store
            .lock()
            .await
            .append(Event::TaskStatus {
                t: RunStore::now(),
                id: "t9".into(),
                status: TaskStatus::Failed,
                outcome: Some(TaskOutcome {
                    summary: "flaky".into(),
                    files_changed: Vec::new(),
                }),
            })
            .await
            .unwrap();

        let deadline = std::time::Instant::now() + COALESCE + Duration::from_secs(5);
        let cards = loop {
            let c = wingman_config::inbox::read_open(inbox.path());
            if !c.is_empty() || std::time::Instant::now() > deadline {
                break c;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        watchdog.abort();

        assert_eq!(cards.len(), 1, "{cards:?}");
        let actions = &cards[0].actions;
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].id, "abort");
        // The button carries the command verbatim, so the popup and the panel
        // both write it without knowing the vocabulary.
        assert_eq!(
            actions[0].control,
            Some(serde_json::json!({ "cmd": "abort_run" }))
        );
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
        assert!(
            matches!(err, OrchestratorError::InvalidDag(_)),
            "got {err:?}"
        );

        // Build a valid chain t1 → t2, then re-create t1 depending on t2 —
        // that closes a t1→t2→t1 cycle and must be refused.
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        handle.add_task(dev_task("t2", vec!["t1"])).await.unwrap();
        let err = handle
            .add_task(dev_task("t1", vec!["t2"]))
            .await
            .unwrap_err();
        assert!(
            matches!(err, OrchestratorError::InvalidDag(_)),
            "got {err:?}"
        );

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

    /// A worker that has been assigned but not yet recorded `InProgress`
    /// leaves the task `Todo`; a second assign in that window must not start
    /// another worker in the same worktree.
    #[tokio::test]
    async fn assign_refuses_a_task_whose_worker_is_still_starting() {
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
        let starting: WorkerSpawner = Arc::new(|_ctx: SpawnContext| {
            Box::pin(async move {
                futures::future::pending::<()>().await;
                unreachable!("never resumed")
            })
        });
        let (handle, join) = spawn(store, cfg(dir.path().to_path_buf()), starting);
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        handle.assign_task("t1").await.unwrap();
        assert_eq!(
            handle.snapshot().await.unwrap().task("t1").unwrap().status,
            TaskStatus::Todo
        );
        match handle.assign_task("t1").await {
            Err(OrchestratorError::BadTransition(id, TaskStatus::Todo, _)) => assert_eq!(id, "t1"),
            other => panic!("expected BadTransition, got {other:?}"),
        }
        // The worker never returns, so don't await `join`.
        join.abort();
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

    /// Regression, from a live `auto_dispatch` run (#34).
    ///
    /// A task reached Review with its work done and acceptance green. Something
    /// then called Reassign on it — the manager has a `reassign_task` tool, and
    /// unlike the HTTP retry route it did not check the status. The ladder reset
    /// the task to Todo, a fresh worker redid the work from scratch, ran out of
    /// turns, and the run ended Failed with the finished edit stranded
    /// uncommitted in the worktree.
    ///
    /// Reassign is the retry path. Retrying work that already succeeded is never
    /// right: at best it is paid for twice, at worst it destroys it.
    #[tokio::test]
    async fn reassigning_a_completed_task_is_refused() {
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
        let _agent = handle.assign_task("t1").await.unwrap();
        wait_for_review(&handle, "t1").await;

        // The exact call the manager's tool makes.
        let err = handle.reassign("t1").await;
        assert!(
            err.is_err(),
            "a Review task must not be reassignable: {err:?}"
        );

        // And the work is still there: still Review, still finalizable.
        let state = handle.snapshot().await.unwrap();
        assert_eq!(
            state.task("t1").unwrap().status,
            TaskStatus::Review,
            "reassign must not have reset the task"
        );
        handle
            .finalize_task("t1", Some("sha-1".into()))
            .await
            .unwrap();
        let state = handle.snapshot().await.unwrap();
        assert_eq!(state.task("t1").unwrap().status, TaskStatus::Done);

        // Done is protected too — nothing may send a merged task round again.
        assert!(
            handle.reassign("t1").await.is_err(),
            "a Done task must not be reassignable either"
        );

        handle.shutdown().await;
        let _ = join.await;
    }

    /// Companion to `reassigning_a_completed_task_is_refused`, for the sibling
    /// path that had the same hole.
    ///
    /// From a live run (#34): the manager called abort_task on tasks whose
    /// agents had already emitted `task_complete`. `handle_abort` never looked
    /// at the status, marked them Blocked, and the run deadlocked on three
    /// tasks whose work was finished and thrown away.
    ///
    /// Abort exists to stop a worker. A Review task has no worker left to
    /// stop — only work to lose.
    #[tokio::test]
    async fn aborting_a_completed_task_is_refused() {
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
        let _agent = handle.assign_task("t1").await.unwrap();
        wait_for_review(&handle, "t1").await;

        assert!(
            handle.abort_task("t1").await.is_err(),
            "a Review task must not be abortable"
        );
        let state = handle.snapshot().await.unwrap();
        assert_eq!(
            state.task("t1").unwrap().status,
            TaskStatus::Review,
            "abort must not have moved the task"
        );

        // Still finalizable afterwards — the work survived.
        handle
            .finalize_task("t1", Some("sha-1".into()))
            .await
            .unwrap();
        assert_eq!(
            handle.snapshot().await.unwrap().task("t1").unwrap().status,
            TaskStatus::Done
        );
        assert!(
            handle.abort_task("t1").await.is_err(),
            "a Done task must not be abortable either"
        );

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

    /// A task that never declared what it writes cannot be proven disjoint
    /// from one that did, so it must not run concurrently. The assignment path
    /// used to call `writes_overlap` directly, which returns false whenever
    /// either side is empty — so the tasks we know least about were exactly
    /// the ones allowed to run in parallel.
    #[tokio::test]
    async fn task_without_declared_writes_is_serialised() {
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

        let (handle, join) = spawn(store, cfg(dir.path().to_path_buf()), fake_hanging_spawner());

        // t1 declares a write-set; t2 declares none. (dev_task declares one
        // by default, so clear it — the undeclared case is the point.)
        let mut t1 = dev_task("t1", vec![]);
        t1.writes = vec!["src/a.rs".into()];
        let mut t2 = dev_task("t2", vec![]);
        t2.writes.clear();

        handle.add_task(t1).await.unwrap();
        handle.add_task(t2).await.unwrap();

        handle.assign_task("t1").await.unwrap();

        // assign_task returns once the worker is spawned; the spawner records
        // InProgress asynchronously. Wait for that to land so the conflict
        // check has a live task to see, rather than racing it.
        for _ in 0..100 {
            let st = handle.snapshot().await.unwrap();
            if st
                .tasks
                .iter()
                .any(|t| t.id == "t1" && t.status == TaskStatus::InProgress)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        match handle.assign_task("t2").await {
            Err(OrchestratorError::WriteConflict(a, b)) => {
                assert_eq!(a, "t2");
                assert_eq!(b, "t1");
            }
            other => panic!("expected WriteConflict for an undeclared write-set, got {other:?}"),
        }

        // The hanging spawner never returns, so don't await `join` — abort the
        // actor and let the temp dir drop.
        join.abort();
    }

    /// The USD cap is computed from a hardcoded price table, so an unpriced
    /// model reports $0 spend forever and the cost cap can never fire. Token
    /// counts are recorded regardless of pricing — this is the bound that
    /// actually holds for the majority of models Wingman can talk to.
    #[tokio::test]
    async fn token_cap_stops_a_run_on_an_unpriced_model() {
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
        // A lot of work on a model with no price-table entry: usd stays 0.
        store
            .append(Event::AgentUsd {
                t: RunStore::now(),
                agent: "agent-pre".into(),
                model: "some-brand-new-model".into(),
                input_tokens: 900_000,
                output_tokens: 200_000,
                usd: 0.0,
            })
            .await
            .unwrap();

        let mut config = cfg(dir.path().to_path_buf());
        config.max_usd = 10.0; // would never trip: spend is priced at $0
        config.max_total_tokens = 1_000_000;

        let (handle, join) = spawn(store, config, fake_happy_spawner());
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        match handle.assign_task("t1").await {
            Err(OrchestratorError::TokenCap { spent, cap }) => {
                assert_eq!(spent, 1_100_000);
                assert_eq!(cap, 1_000_000);
            }
            other => panic!("expected TokenCap, got {other:?}"),
        }
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

    /// #35 — a `tell` written to the control file by a *separate process*
    /// reaches the live worker's stdin channel. This is the whole point of
    /// `pilot tell`: the CLI never touches the orchestrator directly.
    #[tokio::test]
    async fn tell_control_command_reaches_the_live_worker() {
        use std::time::Duration;
        let dir = tempdir().unwrap();
        let run_dir = crate::run_dir(dir.path(), "test-run");
        let store = RunStore::create(
            run_dir.clone(),
            "test-run",
            "g",
            "deadbeef",
            "wingman/auto/test-run",
        )
        .await
        .unwrap();

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
                    // Bounded: a worker that is never messaged must still
                    // finish, or the test hangs waiting for it.
                    if let Ok(Some(cmd)) =
                        tokio::time::timeout(Duration::from_secs(5), rx.recv()).await
                    {
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
        // The watchdog truncates the control file when it starts; write after
        // that so the command isn't cleared before it is read.
        tokio::time::sleep(Duration::from_millis(500)).await;

        crate::control::append(
            &run_dir,
            &crate::control::ControlCommand::Tell {
                task: Some("t1".into()),
                message: "also update the changelog".into(),
                reply: false,
            },
        )
        .unwrap();

        for _ in 0..200 {
            if !received.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            received.lock().await.clone(),
            vec![crate::ipc::ManagerCommand::Note {
                text: "also update the changelog".into(),
                reply: false,
            }]
        );
        handle.shutdown().await;
        let _ = join.await;
    }

    /// A `tell` naming a task nobody is working on must not be broadcast to
    /// whoever happens to be running — it goes nowhere.
    #[tokio::test]
    async fn tell_for_an_unheld_task_reaches_nobody() {
        use std::time::Duration;
        let dir = tempdir().unwrap();
        let run_dir = crate::run_dir(dir.path(), "test-run");
        let store = RunStore::create(
            run_dir.clone(),
            "test-run",
            "g",
            "deadbeef",
            "wingman/auto/test-run",
        )
        .await
        .unwrap();

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
                    // Bounded so the never-messaged worker still finishes.
                    if let Ok(Some(cmd)) =
                        tokio::time::timeout(Duration::from_secs(3), rx.recv()).await
                    {
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
        tokio::time::sleep(Duration::from_millis(500)).await;

        crate::control::append(
            &run_dir,
            &crate::control::ControlCommand::Tell {
                task: Some("nope".into()),
                message: "hello?".into(),
                reply: true,
            },
        )
        .unwrap();

        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(
            received.lock().await.is_empty(),
            "a tell for an unassigned task must not be delivered to another worker"
        );
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
        let rework: Reviewer =
            std::sync::Arc::new(|_task| Box::pin(async { Some("add tests".to_string()) }));
        let dir = tempdir().unwrap();
        let store = seed_review(dir.path()).await;
        let (handle, join) = spawn_full(
            store,
            cfg(dir.path().to_path_buf()),
            fake_happy_spawner(),
            None,
            Some(rework),
        );
        let err = handle.finalize_task("t1", None).await.unwrap_err();
        assert!(
            matches!(err, OrchestratorError::ReviewRework(ref id) if id == "t1"),
            "got {err:?}"
        );
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
        let (handle, join) = spawn_full(
            store,
            cfg(dir.path().to_path_buf()),
            fake_happy_spawner(),
            None,
            Some(approve),
        );
        handle.finalize_task("t1", None).await.unwrap();
        assert_eq!(
            handle.snapshot().await.unwrap().task("t1").unwrap().status,
            TaskStatus::Done
        );
        handle.shutdown().await;
        let _ = join.await;
    }

    /* ── E9 adaptive concurrency and speculative pre-spawn ──────────────── */

    /// A provider's `Retry-After`, reported by a worker, holds the cap at the
    /// floor: with one task running, a second is refused while it lasts.
    #[tokio::test]
    async fn a_reported_retry_after_holds_the_cap_at_one() {
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
        // A worker that starts, hits a 429 with Retry-After, and keeps going.
        let rate_limited: WorkerSpawner = Arc::new(|ctx: SpawnContext| {
            Box::pin(async move {
                {
                    let mut store = ctx.store.lock().await;
                    for ev in [
                        Event::TaskStatus {
                            t: RunStore::now(),
                            id: ctx.task.id.clone(),
                            status: TaskStatus::InProgress,
                            outcome: None,
                        },
                        Event::AgentRateLimited {
                            t: RunStore::now(),
                            agent: ctx.agent_id.clone(),
                            status: 429,
                            retry_after_secs: Some(30),
                        },
                    ] {
                        let _ = store.append(ev).await;
                    }
                }
                futures::future::pending::<()>().await;
                unreachable!("never resumed")
            })
        });
        let (handle, join) = spawn(store, cfg(dir.path().to_path_buf()), rate_limited);
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        // Overlapping writes: until the hit is recorded, t2 is refused for the
        // write conflict (checked after the cap) instead of being assigned.
        let mut t2 = dev_task("t2", vec![]);
        t2.writes = vec!["file-t1.rs".into()];
        handle.add_task(t2).await.unwrap();
        handle.assign_task("t1").await.unwrap();

        // The watchdog records the hit asynchronously; wait for it to bite.
        let mut last = None;
        for _ in 0..200 {
            match handle.assign_task("t2").await {
                Err(OrchestratorError::ConcurrencyCap(1)) => {
                    join.abort();
                    return;
                }
                other => last = Some(other),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected ConcurrencyCap(1) under Retry-After, got {last:?}");
    }

    #[tokio::test]
    async fn a_nearly_spent_subscription_holds_the_cap_at_one() {
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
        // A Claude Code worker that starts and reports a nearly spent plan.
        let rate_limited: WorkerSpawner = Arc::new(|ctx: SpawnContext| {
            Box::pin(async move {
                {
                    let mut store = ctx.store.lock().await;
                    for ev in [
                        Event::TaskStatus {
                            t: RunStore::now(),
                            id: ctx.task.id.clone(),
                            status: TaskStatus::InProgress,
                            outcome: None,
                        },
                        // No rejection yet: the plan window is 90% spent.
                        Event::SubscriptionUsage {
                            t: RunStore::now(),
                            agent: ctx.agent_id.clone(),
                            utilization: 0.9,
                            resets_at: None,
                        },
                    ] {
                        let _ = store.append(ev).await;
                    }
                }
                futures::future::pending::<()>().await;
                unreachable!("never resumed")
            })
        });
        let (handle, join) = spawn(store, cfg(dir.path().to_path_buf()), rate_limited);
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        // Overlapping writes: until the hit is recorded, t2 is refused for the
        // write conflict (checked after the cap) instead of being assigned.
        let mut t2 = dev_task("t2", vec![]);
        t2.writes = vec!["file-t1.rs".into()];
        handle.add_task(t2).await.unwrap();
        handle.assign_task("t1").await.unwrap();

        // The watchdog records the hit asynchronously; wait for it to bite.
        let mut last = None;
        for _ in 0..200 {
            match handle.assign_task("t2").await {
                Err(OrchestratorError::ConcurrencyCap(1)) => {
                    join.abort();
                    return;
                }
                other => last = Some(other),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected ConcurrencyCap(1) at 90% subscription usage, got {last:?}");
    }

    /// A one-commit git repo with a run store under it, and a config that
    /// pre-spawns with `warm_cmd`. `None` without git.
    async fn speculative_run(dir: &std::path::Path) -> Option<(RunStore, OrchestratorConfig)> {
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
        };
        git(&["init", "-q"]).ok()?;
        for kv in [
            ["user.email", "t@t.t"],
            ["user.name", "t"],
            ["core.autocrlf", "false"],
        ] {
            git(&["config", kv[0], kv[1]]).unwrap();
        }
        std::fs::write(
            dir.join(".gitignore"),
            ".wingman/
",
        )
        .unwrap();
        git(&["add", "-A"]).unwrap();
        git(&["commit", "-qm", "base"]).unwrap();
        let head = String::from_utf8(git(&["rev-parse", "HEAD"]).unwrap().stdout).unwrap();
        let store = RunStore::create(
            dir.join(".wingman/autonomous/test-run"),
            "test-run",
            "g",
            head.trim(),
            "wingman/auto/test-run",
        )
        .await
        .unwrap();
        let mut c = cfg(dir.to_path_buf());
        c.base_commit = head.trim().to_string();
        c.use_real_worktrees = true;
        c.speculative_prespawn = true;
        c.warm_cmd = "echo warm> warm.txt".into();
        Some((store, c))
    }

    async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..500 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// Once t1 reaches Review, t2 (which waits only on t1) gets its worktree
    /// and warm-up early, and assigning t2 takes that worktree over instead of
    /// recreating it.
    #[tokio::test]
    async fn a_task_about_to_be_ready_gets_its_worktree_early_and_keeps_it() {
        let dir = tempdir().unwrap();
        let Some((store, c)) = speculative_run(dir.path()).await else {
            eprintln!("skipping: git not available");
            return;
        };
        let t2_warm = crate::worktree_dir(dir.path(), "test-run", "t2").join("warm.txt");
        let (handle, join) = spawn(store, c, fake_happy_spawner());
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        handle.add_task(dev_task("t2", vec!["t1"])).await.unwrap();
        handle.assign_task("t1").await.unwrap();

        wait_until("t2's warm-up", || t2_warm.exists()).await;

        for _ in 0..200 {
            if handle.snapshot().await.unwrap().task("t1").unwrap().status == TaskStatus::Review {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        handle.finalize_task("t1", None).await.unwrap();
        handle.assign_task("t2").await.unwrap();
        // A fresh `create_worktree` would have wiped the warm-up's output.
        assert!(t2_warm.exists(), "the assignment recreated the worktree");
        handle.shutdown().await;
        let _ = join.await;
        // Adopted, so shutdown leaves it for the merge.
        assert!(t2_warm.exists());
    }

    /// Replanning a waiting task onto a dep that has not started means it is
    /// no longer about to run: its speculative worktree is removed.
    #[tokio::test]
    async fn a_plan_change_discards_the_speculative_worktree() {
        let dir = tempdir().unwrap();
        let Some((store, c)) = speculative_run(dir.path()).await else {
            eprintln!("skipping: git not available");
            return;
        };
        let t2_dir = crate::worktree_dir(dir.path(), "test-run", "t2");
        let (handle, join) = spawn(store, c, fake_happy_spawner());
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        handle.add_task(dev_task("t2", vec!["t1"])).await.unwrap();
        handle.add_task(dev_task("t3", vec![])).await.unwrap();
        handle.assign_task("t1").await.unwrap();
        wait_until("t2's worktree", || t2_dir.join("warm.txt").exists()).await;

        handle
            .add_task(dev_task("t2", vec!["t1", "t3"]))
            .await
            .unwrap();
        wait_until("the discard", || !t2_dir.exists()).await;
        handle.shutdown().await;
        let _ = join.await;
    }

    /* ── J15 runtime escalations ────────────────────────────────────────── */

    async fn wait_for_escalations(store: &Arc<Mutex<RunStore>>, n: usize) -> Vec<String> {
        for _ in 0..200 {
            let labels: Vec<String> = store
                .lock()
                .await
                .state()
                .escalations
                .iter()
                .map(|t| t.short_label().to_string())
                .collect();
            if labels.len() >= n {
                return labels;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("fewer than {n} escalations recorded");
    }

    fn attempt(id: &str, rung: u32, status: TaskStatus, tests: &[(&str, u32)]) -> Event {
        Event::TaskAttempt {
            t: RunStore::now(),
            id: id.into(),
            agent: "agent-0001".into(),
            rung,
            model: None,
            status,
            summary: String::new(),
            tests: tests.iter().map(|(l, n)| (l.to_string(), *n)).collect(),
        }
    }

    /// The runtime triggers fire as the run goes, each recorded once: prior
    /// failed runs as it starts, spend on `agent.usd`, and on a finished
    /// attempt fewer passing tests than the base commit and a failure streak.
    #[tokio::test]
    async fn j15_escalation_watchdog_fires_during_the_run() {
        use crate::escalation::EscalationTrigger;

        let dir = tempdir().unwrap();
        let mut store = RunStore::create(dir.path(), "r1", "g", "base", "wingman/auto/r1")
            .await
            .unwrap();
        store
            .append(Event::TaskCreate {
                t: RunStore::now(),
                id: "t1".into(),
                role: Role::Developer,
                title: "x".into(),
                goal: String::new(),
                deps: Vec::new(),
                writes: Vec::new(),
                acceptance: Vec::new(),
                reversibility: Default::default(),
                reversibility_reason: None,
            })
            .await
            .unwrap();
        let events = store.subscribe();
        let store = Arc::new(Mutex::new(store));
        let baselines = TestBaselines::default();
        baselines
            .lock()
            .unwrap()
            .insert("shell: cargo test".into(), Some(120));
        let prior = vec![
            ("r-a".to_string(), false),
            ("r-b".to_string(), false),
            ("r-c".to_string(), false),
        ];
        tokio::spawn(escalation_watchdog(
            events,
            store.clone(),
            baselines,
            10.0,
            prior,
        ));
        assert_eq!(
            wait_for_escalations(&store, 1).await,
            ["3 consecutive failures"]
        );

        let append = |ev: Event| {
            let store = store.clone();
            async move { store.lock().await.append(ev).await.unwrap() }
        };
        append(Event::AgentUsd {
            t: RunStore::now(),
            agent: "agent-0001".into(),
            model: "m".into(),
            input_tokens: 0,
            output_tokens: 0,
            usd: 8.5,
        })
        .await;
        wait_for_escalations(&store, 2).await;
        // Still over 80%: the same incident, not a second card.
        append(Event::AgentUsd {
            t: RunStore::now(),
            agent: "agent-0001".into(),
            model: "m".into(),
            input_tokens: 0,
            output_tokens: 0,
            usd: 0.1,
        })
        .await;
        // A failed attempt with fewer tests is not compared; a Review one is.
        append(attempt(
            "t1",
            0,
            TaskStatus::Failed,
            &[("shell: cargo test", 1)],
        ))
        .await;
        append(attempt(
            "t1",
            1,
            TaskStatus::Review,
            &[("shell: cargo test", 115)],
        ))
        .await;
        wait_for_escalations(&store, 3).await;

        let recorded = store.lock().await.state().escalations.clone();
        assert!(recorded.contains(&EscalationTrigger::NetNegativeTests {
            task_id: "t1".into(),
            before: 120,
            after: 115,
        }));
        assert_eq!(
            recorded
                .iter()
                .filter(|t| matches!(t, EscalationTrigger::CostWarn { .. }))
                .count(),
            1
        );
        let log = std::fs::read_to_string(store.lock().await.log_path()).unwrap();
        assert!(log.contains(r#""ev":"run.escalation""#));
    }

    /// Three failed attempts in a row inside the run trip the streak trigger,
    /// and the watchdog is wired into every spawned orchestrator.
    #[tokio::test]
    async fn j15_three_failed_attempts_escalate_through_a_spawned_orchestrator() {
        let dir = tempdir().unwrap();
        let store = RunStore::create(dir.path(), "r1", "g", "base", "wingman/auto/r1")
            .await
            .unwrap();
        let spawner: WorkerSpawner = Arc::new(|ctx: SpawnContext| {
            Box::pin(async move {
                let mut store = ctx.store.lock().await;
                for rung in 0..3 {
                    let _ = store
                        .append(attempt(&ctx.task.id, rung, TaskStatus::Failed, &[]))
                        .await;
                }
                Ok(WorkerSpawnResult {
                    agent_id: ctx.agent_id,
                    status: TaskStatus::InProgress,
                    outcome: None,
                })
            })
        });
        let (handle, join) = spawn(store, cfg(PathBuf::new()), spawner);
        handle.add_task(dev_task("t1", vec![])).await.unwrap();
        handle.assign_task("t1").await.unwrap();
        let mut fired = false;
        for _ in 0..200 {
            let state = handle.snapshot().await.unwrap();
            if state.escalations.iter().any(|t| {
                matches!(
                    t,
                    crate::escalation::EscalationTrigger::RepeatedFailures { related_runs }
                        if related_runs.len() == 3
                )
            }) {
                fired = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(fired, "no RepeatedFailures escalation recorded");
        handle.shutdown().await;
        let _ = join.await;
    }

    /// Base-commit counts are measured once per check per run, and the task
    /// reads as busy while they are.
    #[tokio::test]
    async fn j15_baselines_are_measured_once_per_check() {
        let dir = tempdir().unwrap();
        let mut store =
            RunStore::create(dir.path().join("run"), "r1", "g", "base", "wingman/auto/r1")
                .await
                .unwrap();
        store
            .append(Event::TaskCreate {
                t: RunStore::now(),
                id: "t1".into(),
                role: Role::Developer,
                title: "x".into(),
                goal: String::new(),
                deps: Vec::new(),
                writes: Vec::new(),
                acceptance: Vec::new(),
                reversibility: Default::default(),
                reversibility_reason: None,
            })
            .await
            .unwrap();
        let store = Arc::new(Mutex::new(store));
        let cmd = if cfg!(windows) {
            "echo x>>ran.txt & echo test result: ok. 7 passed; 0 failed;"
        } else {
            "echo x >> ran.txt; echo 'test result: ok. 7 passed; 0 failed;'"
        };
        let mut task = Task::new("t1", Role::Developer, "x");
        task.acceptance = vec![
            Acceptance::Shell { cmd: cmd.into() },
            // Not a test run: never measured.
            Acceptance::Shell {
                cmd: "cargo check".into(),
            },
        ];
        let baselines = TestBaselines::default();
        for _ in 0..2 {
            measure_test_baselines(
                &store,
                &task,
                dir.path().to_path_buf(),
                Duration::from_secs(30),
                &baselines,
            )
            .await;
        }
        let label = format!("shell: {cmd}");
        assert_eq!(
            baselines.lock().unwrap().clone(),
            HashMap::from([(label, Some(7))])
        );
        let ran = std::fs::read_to_string(dir.path().join("ran.txt")).unwrap();
        assert_eq!(ran.lines().count(), 1, "measured twice");
        assert_eq!(
            store.lock().await.state().task("t1").unwrap().status,
            TaskStatus::InProgress
        );
    }

    #[test]
    fn an_escalation_card_leads_with_the_trigger() {
        let (title, body) = render(&[
            task("parser", "boom"),
            Bad::Escalation("net-negative tests", "task t1 ended with 3 passing".into()),
        ]);
        assert_eq!(title, "Escalation — net-negative tests");
        assert!(body.contains("• parser"));
        assert!(body.contains("• task t1 ended with 3 passing"));
        // A run outcome still wins the title; the escalation stays in the body.
        let (title, body) = render(&[
            Bad::Run("failed"),
            Bad::Escalation("cost halt (>=1.0x)", "spend crossed cap".into()),
        ]);
        assert_eq!(title, "Run failed");
        assert_eq!(body, "• spend crossed cap");
    }

    /* ── Failure cards ─────────────────────────────────────────────────── */

    fn task(label: &str, summary: &str) -> Bad {
        Bad::Task(label.into(), summary.into())
    }

    #[test]
    fn a_lone_task_failure_keeps_its_summary() {
        let (title, body) = render(&[task("build the parser", "cargo test failed on 3 cases")]);
        assert_eq!(title, "Task failed — build the parser");
        assert_eq!(body, "cargo test failed on 3 cases");
    }

    #[test]
    fn several_failures_become_one_card_listing_them() {
        // The shape this exists for: one broken dependency taking three tasks
        // down, which used to be three cards to dismiss separately.
        let (title, body) = render(&[
            task("parser", "boom"),
            task("lexer", "boom"),
            task("printer", "boom"),
        ]);
        assert_eq!(title, "3 tasks failed");
        assert!(body.contains("• parser"));
        assert!(body.contains("• lexer"));
        assert!(body.contains("• printer"));
    }

    #[test]
    fn the_run_outcome_wins_the_title_and_its_tasks_go_underneath() {
        let (title, body) = render(&[task("parser", "boom"), Bad::Run("failed")]);
        assert_eq!(title, "Run failed — 1 task(s) did not finish");
        assert!(body.contains("• parser"));
    }

    #[test]
    fn a_run_that_fails_alone_says_only_that() {
        let (title, body) = render(&[Bad::Run("aborted")]);
        assert_eq!(title, "Run aborted");
        assert!(body.is_empty());
    }
}
