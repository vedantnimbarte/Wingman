//! Worker supervision — the parent half of the pilot worker subprocess.
//!
//! The orchestrator launches one [`WorkerHandle`] per scheduled task. Each
//! handle:
//!
//! 1. Spawns `wingman --worker-mode --task-file … --role … --worktree …
//!    --print --json` under a [`crate::child_process::Supervisor`] (cross-
//!    platform tree-kill).
//! 2. Parses stdout line-by-line. Each line is either:
//!    - an `AgentEvent` produced by `wingman-core` (tool start / result,
//!      usage, stop, error), or
//!    - the synthetic `worker_start` / `task_complete` markers emitted by
//!      the worker shim.
//! 3. Forwards a small subset (tool starts, usage deltas, completion) into
//!    the [`crate::RunStore`] so the dashboard sees live progress.
//! 4. Enforces `pilot.task_timeout_secs` — on expiry the supervisor tree-
//!    kills the child and the task is marked `failed` for the retry ladder
//!    to pick up.

use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::timeout;

use crate::child_process::{SupervisedCommand, Supervisor, SupervisorError};
use crate::model::{AgentStatus, Event, Role, Task, TaskOutcome, TaskStatus};
use crate::store::{RunStore, StoreError};

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("supervisor: {0}")]
    Supervisor(#[from] SupervisorError),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("serde_json: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("worker subprocess exited with status {0:?} before reporting task_complete")]
    EarlyExit(Option<i32>),
    #[error("worker task timed out after {0:?}")]
    Timeout(Duration),
    #[error("sandbox: {0}")]
    Sandbox(String),
}

/// Spec for one worker launch. All paths absolute; relative paths confuse
/// `current_dir` once the supervised child runs.
// Not Clone/Debug — carries a one-shot IPC command Receiver (E10).
pub struct WorkerSpec {
    /// Path to the wingman binary. Usually `std::env::current_exe()`.
    pub wingman_bin: PathBuf,
    /// The task this worker will run.
    pub task: Task,
    /// Worker role.
    pub role: Role,
    /// Worktree (cwd) for the worker.
    pub worktree: PathBuf,
    /// Session id for the worker's own JSONL log.
    pub session_id: String,
    /// Optional model override forwarded as `--model`.
    pub model: Option<String>,
    /// Hard timeout for the whole task.
    pub timeout: Duration,
    /// E10 — receive end of the manager→worker command channel. When set,
    /// `run_worker` drains it into the child's stdin (one `encode_command`
    /// line per command). `None` leaves the channel unused (stdin is closed
    /// so the child sees EOF).
    pub cmd_rx: Option<tokio::sync::mpsc::Receiver<crate::ipc::ManagerCommand>>,
    /// E5 retry-ladder rung this attempt runs on (`SpawnContext::rung`),
    /// recorded in the attempt's `task.attempt` event.
    pub rung: u32,
    /// E5.5 — consecutive failures of the worker's turn gate after which it
    /// restores the worktree to the last state that passed the gate. 0 turns
    /// rollback off. Forwarded as `--turn-rollback-after`.
    pub turn_rollback_after: u32,
    /// E11 — hold the attempt out of Review unless its recorded tool calls
    /// satisfy checkpoint hygiene ([`crate::checkpoint::verify`]), and tell the
    /// worker so in its prompt. Forwarded as `--checkpoint-hygiene`.
    pub checkpoint_hygiene: bool,
    /// J11 — run the worker in a container or Firecracker VM against a copy
    /// of `worktree`, and apply its diff back when it completes. `None` runs
    /// it on the host.
    pub sandbox: Option<crate::sandbox::WorkerSandbox>,
    /// J7 — register `propose_tool` on the worker, approving its proposals
    /// at this tier ([`crate::approval::tool_synthesis_tier`]). `None` leaves
    /// tool synthesis off. Ignored for a sandboxed worker, which runs against
    /// a copy with no `.wingman/` and no trust store to write to.
    pub tool_synthesis: Option<crate::approval::ApprovalTier>,
}

/// Live handle returned by [`spawn_worker`]. Owns the supervised child and
/// the parser task draining its stdout.
pub struct WorkerHandle {
    pub task_id: String,
    pub agent_id: String,
    pub supervisor: Supervisor,
    /// Outcome reported via the `task_complete` marker, set once it arrives.
    pub outcome: Option<TaskOutcome>,
}

/// Final result of a worker run.
#[derive(Debug, Clone)]
pub struct WorkerResult {
    pub task_id: String,
    pub agent_id: String,
    pub status: TaskStatus,
    pub outcome: Option<TaskOutcome>,
    pub exit_code: Option<i32>,
}

/// Spawn one worker, drive it to completion, and update `store` along the
/// way. The function returns once the worker exits or the timeout fires.
///
/// `agent_id` lets the caller link this worker to a `task.assign` event it
/// has already emitted. The orchestrator chooses the id; the worker just
/// inherits it.
pub async fn run_worker(
    store: &tokio::sync::Mutex<RunStore>,
    agent_id: &str,
    mut spec: WorkerSpec,
) -> Result<WorkerResult, WorkerError> {
    // Write the task spec to a temp file the child will read. We use a
    // file rather than stdin so the worker's stdin stays free for the IPC
    // command channel (E10).
    let task_path = write_task_file(&spec.task, &spec.worktree)?;

    // J11 — a sandboxed worker is the same `wingman --worker-mode`, run by
    // docker/firecracker against a copy, with its paths as the guest sees
    // them. Preparing copies the worktree (and packs a drive for a VM), so
    // it runs off the async threads.
    let sandbox_run = match spec.sandbox.clone() {
        None => None,
        Some(sb) => {
            let guest = crate::sandbox::GUEST_WORK;
            let args: Vec<String> = worker_args(
                &spec,
                format!("{guest}/.wingman/pilot/task-{}.json", spec.task.id).as_ref(),
                guest.as_ref(),
                None,
            )
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
            let (worktree, session) = (spec.worktree.clone(), spec.session_id.clone());
            let prepared = tokio::task::spawn_blocking(move || {
                crate::sandbox::prepare(
                    &sb,
                    &worktree,
                    &session,
                    &args,
                    &crate::pr::SystemCommandRunner,
                    &std::env::temp_dir(),
                )
            })
            .await
            .unwrap_or_else(|e| Err(format!("sandbox setup panicked: {e}")));
            match prepared {
                Ok(run) => Some(run),
                Err(e) => {
                    let summary = format!("could not prepare the sandbox: {e}");
                    record_failure(store, &spec, agent_id, summary.clone()).await;
                    return Err(WorkerError::Sandbox(summary));
                }
            }
        }
    };

    let mut sc = match &sandbox_run {
        Some(run) => {
            let mut sc = SupervisedCommand::new(&run.program);
            sc.command_mut().args(&run.args);
            sc
        }
        None => {
            let mut sc = SupervisedCommand::new(&spec.wingman_bin);
            sc.command_mut().args(worker_args(
                &spec,
                task_path.as_os_str(),
                spec.worktree.as_os_str(),
                spec.tool_synthesis,
            ));
            sc
        }
    };
    sc.command_mut().current_dir(&spec.worktree);

    let mut supervisor = sc.spawn()?;
    let pid = supervisor.pid();

    // The run store is shared across all workers, the orchestrator actor, and
    // the budget watchdog. Lock it only for the duration of each append — the
    // worker spends almost all its wall-clock awaiting child stdout, and
    // holding the guard across that would serialize every other worker (and
    // stall the manager loop) to an effective concurrency of one.
    let _ = store
        .lock()
        .await
        .append(Event::AgentSpawn {
            t: RunStore::now(),
            agent: agent_id.to_string(),
            role: spec.role.clone(),
            pid: Some(pid),
            session_id: Some(spec.session_id.clone()),
        })
        .await;

    let child = supervisor
        .take_child()
        .ok_or(WorkerError::EarlyExit(None))?;

    // Parse stdout NDJSON line by line. The child still owns stdout; we
    // move it out.
    let mut child = child;
    let stdout = child.stdout.take().ok_or(WorkerError::EarlyExit(None))?;
    let stderr = child.stderr.take();

    // E10 — drain the manager→worker command channel into the child's stdin
    // as newline-delimited IPC commands. When there's no channel, drop the
    // stdin handle so the child reads EOF and its own stdin reader exits.
    if let Some(stdin) = child.stdin.take() {
        if let Some(mut cmd_rx) = spec.cmd_rx.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let mut stdin = stdin;
                while let Some(cmd) = cmd_rx.recv().await {
                    let line = format!("{}\n", crate::ipc::encode_command(&cmd));
                    if stdin.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = stdin.flush().await;
                }
                // Channel closed → drop stdin → child sees EOF.
            });
        }
        // else: `stdin` drops here, closing the pipe (EOF for the child).
    }

    let mut reader = BufReader::new(stdout).lines();

    // Drain stderr in the background so the child doesn't block on a full
    // pipe. We just log it; the structured events live in stdout.
    if let Some(stderr) = stderr {
        let mut err_lines = BufReader::new(stderr).lines();
        let task_id_for_log = spec.task.id.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = err_lines.next_line().await {
                tracing::debug!(target: "pilot::worker", task = %task_id_for_log, "{line}");
            }
        });
    }

    let parse_loop = async {
        let mut outcome: Option<TaskOutcome> = None;
        let mut acceptance: Vec<crate::acceptance::AcceptanceResult> = Vec::new();
        // Why the worker's agent loop stopped. It is already on the wire as
        // an `AgentEvent`; the supervisor used to forward it and forget it,
        // which is how "ran out of turns" and "finished and said nothing"
        // became the same unhelpful message.
        let mut stop: Option<wingman_core::AgentStop> = None;
        while let Some(line) = reader.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // E10 — worker→manager leg: an IPC `WorkerMessage` line
            // (question/ack/blocked) is recorded as an event for visibility
            // and does not flow through the normal task-event parser.
            // `parse_message` returns Ok(None) for ordinary event lines, so
            // the same stdout stream carries both.
            if let Ok(Some(msg)) = crate::ipc::parse_message(line) {
                let _ = store
                    .lock()
                    .await
                    .append(Event::TaskTool {
                        t: RunStore::now(),
                        id: spec.task.id.clone(),
                        agent: agent_id.to_string(),
                        tool: format!("worker_msg:{}", crate::ipc::encode_message(&msg)),
                        input_hash: None,
                        file: None,
                        ok: true,
                    })
                    .await;
                continue;
            }
            match parse_line(line) {
                WorkerLine::AgentEvent(ev) => {
                    if let wingman_core::AgentEvent::Stop { reason } = &ev {
                        stop = Some(*reason);
                    }
                    let mut guard = store.lock().await;
                    forward_agent_event(
                        &mut guard,
                        &spec.task.id,
                        agent_id,
                        spec.model.as_deref().unwrap_or_default(),
                        &ev,
                    )
                    .await;
                }
                WorkerLine::WorkerStart { .. } => {
                    let _ = store
                        .lock()
                        .await
                        .append(Event::AgentStatus {
                            t: RunStore::now(),
                            agent: agent_id.to_string(),
                            status: AgentStatus::InProgress,
                        })
                        .await;
                    let _ = store
                        .lock()
                        .await
                        .append(Event::TaskStatus {
                            t: RunStore::now(),
                            id: spec.task.id.clone(),
                            status: TaskStatus::InProgress,
                            outcome: None,
                        })
                        .await;
                }
                WorkerLine::TaskComplete {
                    outcome: o,
                    acceptance: a,
                } => {
                    outcome = Some(o);
                    acceptance = a;
                }
                WorkerLine::RateLimited {
                    status,
                    retry_after_secs,
                } => {
                    let _ = store
                        .lock()
                        .await
                        .append(Event::AgentRateLimited {
                            t: RunStore::now(),
                            agent: agent_id.to_string(),
                            status,
                            retry_after_secs,
                        })
                        .await;
                }
                WorkerLine::SubscriptionUsage {
                    utilization,
                    resets_at,
                } => {
                    let _ = store
                        .lock()
                        .await
                        .append(Event::SubscriptionUsage {
                            t: RunStore::now(),
                            agent: agent_id.to_string(),
                            utilization,
                            resets_at,
                        })
                        .await;
                }
                WorkerLine::Unknown => {
                    tracing::debug!(target: "pilot::worker", "unrecognised worker line: {line}");
                }
            }
        }
        Ok::<
            (
                Option<TaskOutcome>,
                Vec<crate::acceptance::AcceptanceResult>,
                Option<wingman_core::AgentStop>,
            ),
            WorkerError,
        >((outcome, acceptance, stop))
    };

    let (outcome, acceptance, stop) = match timeout(spec.timeout, parse_loop).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            // The stream died mid-run. Say so in the run log: this used to
            // return without recording anything, leaving the task stuck at
            // `in_progress` with a dead worker until the manager reassigned
            // it -- and the next rung was handed "failed without outcome
            // summary", which is not something anyone can act on.
            record_failure(
                store,
                &spec,
                agent_id,
                format!("worker stream ended abnormally: {e}"),
            )
            .await;
            return Err(e);
        }
        Err(_) => {
            supervisor
                .terminate(Duration::from_secs(2).min(spec.timeout))
                .await
                .ok();
            record_failure(
                store,
                &spec,
                agent_id,
                format!(
                    "worker exceeded pilot.task_timeout_secs ({}s) and was terminated",
                    spec.timeout.as_secs()
                ),
            )
            .await;
            return Err(WorkerError::Timeout(spec.timeout));
        }
    };

    let status = match child.wait().await {
        Ok(s) => s,
        Err(e) => {
            record_failure(
                store,
                &spec,
                agent_id,
                format!("could not reap the worker process: {e}"),
            )
            .await;
            return Err(e.into());
        }
    };
    let exit_code = status.code();

    // J11 patch-back. Only a worker that would pass the E3 gate (clean exit,
    // `task_complete`, green acceptance) gets its diff applied: a failed
    // attempt leaves the host worktree exactly as it was. Committed here
    // because the squash-merge reads the task branch, and the worker's own
    // commits stayed in the copy. The task file goes first so that commit
    // cannot pick it up.
    if let Some(run) = sandbox_run {
        let passes = compute_final_status(
            &outcome,
            status.success(),
            &spec.task.acceptance,
            &acceptance,
        ) == TaskStatus::Review;
        if passes {
            let _ = std::fs::remove_file(&task_path);
            let worktree = spec.worktree.clone();
            let message = format!("pilot({}): sandboxed worker changes", spec.task.id);
            let applied = tokio::task::spawn_blocking(move || {
                crate::sandbox::patch_back(&run, &worktree, &crate::pr::SystemCommandRunner)
                    .and_then(|changed| {
                        if changed {
                            crate::worktree::commit_checkpoint(&worktree, &message)
                                .map_err(|e| e.to_string())?;
                        }
                        Ok(())
                    })
            })
            .await
            .unwrap_or_else(|e| Err(format!("patch-back panicked: {e}")));
            if let Err(e) = applied {
                let summary = format!("sandbox patch-back failed: {e}");
                record_failure(store, &spec, agent_id, summary.clone()).await;
                return Err(WorkerError::Sandbox(summary));
            }
        }
    }

    // Salvage a silent-success worker. A worker can do everything right —
    // edit files, commit, pass every acceptance check — yet stop on
    // `max_turns` (or simply forget) without calling `task_complete`. Without
    // that terminal signal `outcome` is None and the E3 gate below throws the
    // correct, committed work away as Failed (then the run wastes tokens
    // retrying it). When the worker exited cleanly and the task actually
    // declared checks, re-run them authoritatively against the worktree: if
    // they're all green, synthesize the outcome the worker never sent. This
    // doubles as a trust check — the parent now verifies acceptance itself
    // rather than taking the worker's self-report on faith.
    // Never for a sandboxed worker: re-running its acceptance commands here
    // would run them on the host, which is what the sandbox exists to avoid.
    let (outcome, acceptance) = if spec.sandbox.is_none()
        && should_reverify(outcome.is_some(), status.success(), &spec.task.acceptance)
    {
        // Bounded by the task's own timeout rather than a constant: this is
        // usually the first compile in a brand-new worktree, and judging it
        // against 60s produced a red check for a tree that builds fine.
        let verified = crate::acceptance::run_acceptance_checks_within(
            &spec.task.acceptance,
            &spec.worktree,
            spec.timeout,
        );
        if crate::acceptance::all_green(&verified) {
            tracing::info!(
                target: "pilot::worker",
                task = %spec.task.id,
                "worker stopped without task_complete but acceptance is green; salvaging to Review"
            );
            (
                Some(TaskOutcome {
                    summary: "completed without explicit task_complete; \
                                  acceptance re-verified green by the supervisor"
                        .into(),
                    files_changed: spec.task.writes.clone(),
                }),
                verified,
            )
        } else {
            // Not green. Keep `verified` rather than the empty vector we came
            // in with: those results are the only record of which check
            // failed, and dropping them left the failure summary with nothing
            // to say.
            (outcome, verified)
        }
    } else {
        (outcome, acceptance)
    };

    // E3 gate: if the task declared acceptance checks, the worker MUST
    // have returned green results in order to move to Review. Otherwise
    // the task lands in Failed for the retry watchdog to pick up.
    let mut final_status = compute_final_status(
        &outcome,
        status.success(),
        &spec.task.acceptance,
        &acceptance,
    );
    let acceptance_green = matches!(final_status, TaskStatus::Review);
    if !acceptance_green {
        tracing::warn!(
            target: "pilot::worker",
            task = %spec.task.id,
            summary = %crate::acceptance::summarize(&acceptance),
            "acceptance checks failed; gating to Failed (E3)"
        );
    }

    // E11 gate: green multi-file work that never checkpointed does not enter
    // Review either. It fails like a red check, so the retry ladder hands the
    // next attempt the reason.
    let hygiene_violation = if spec.checkpoint_hygiene && acceptance_green {
        checkpoint_violation(store, &spec.task.id).await
    } else {
        None
    };
    if let Some(reason) = &hygiene_violation {
        tracing::warn!(target: "pilot::worker", task = %spec.task.id, "{reason}; gating to Failed (E11)");
        final_status = TaskStatus::Failed;
    }

    // A worker that fails the E3 gate without calling `task_complete` has no
    // outcome of its own, and the acceptance verdict -- the one thing that
    // explains the failure -- was only ever logged. Carry it into the record
    // so the retry ladder, the dashboard and a human all get told why.
    let recorded_outcome = match (&outcome, final_status) {
        (Some(o), _) => Some(o.clone()),
        (None, TaskStatus::Failed) => Some(TaskOutcome {
            summary: failure_summary(&acceptance, &spec.task.acceptance, exit_code, stop),
            // What the attempt actually touched, not an assumption that a
            // failure means nothing happened. A worker that ran out of turns
            // has usually written real work, and reporting `[]` next to a
            // modified worktree is how that work becomes invisible.
            files_changed: crate::worktree::changed_files(&spec.worktree),
        }),
        (None, _) => None,
    };
    let recorded_outcome = match hygiene_violation {
        Some(reason) => Some(TaskOutcome {
            summary: format!(
                "{reason}; the work itself reported: {}",
                recorded_outcome
                    .as_ref()
                    .map_or("nothing", |o| o.summary.as_str())
            ),
            files_changed: recorded_outcome
                .map(|o| o.files_changed)
                .unwrap_or_default(),
        }),
        None => recorded_outcome,
    };

    record_attempt(
        store,
        &spec,
        agent_id,
        final_status,
        recorded_outcome
            .as_ref()
            .map(|o| o.summary.clone())
            .unwrap_or_default(),
        &acceptance,
    )
    .await;
    let _ = store
        .lock()
        .await
        .append(Event::TaskStatus {
            t: RunStore::now(),
            id: spec.task.id.clone(),
            status: final_status,
            outcome: recorded_outcome.clone(),
        })
        .await;
    let _ = store
        .lock()
        .await
        .append(Event::AgentStatus {
            t: RunStore::now(),
            agent: agent_id.to_string(),
            status: if final_status == TaskStatus::Failed {
                AgentStatus::Failed
            } else {
                AgentStatus::Done
            },
        })
        .await;

    // Best-effort cleanup of the task file the parent staged.
    let _ = std::fs::remove_file(&task_path);

    Ok(WorkerResult {
        task_id: spec.task.id,
        agent_id: agent_id.to_string(),
        status: final_status,
        outcome: recorded_outcome,
        exit_code,
    })
}

/// E11 — why the latest attempt on `task_id` may not enter Review under
/// checkpoint hygiene, or `None` when it may. Reads the attempt's `task.tool`
/// events, which the stdout parser has finished writing by the time the
/// attempt ends. An unreadable log lets the attempt through: the gate is about
/// recoverability, and failing finished work over a log read would cost more
/// than it protects.
async fn checkpoint_violation(
    store: &tokio::sync::Mutex<RunStore>,
    task_id: &str,
) -> Option<String> {
    let events = match store.lock().await.read_events().await {
        Ok(events) => events,
        Err(e) => {
            tracing::warn!(target: "pilot::worker", task = %task_id, "checkpoint hygiene not checked: {e}");
            return None;
        }
    };
    match crate::checkpoint::verify(&crate::checkpoint::tool_calls_for_task(&events, task_id)) {
        crate::checkpoint::CheckpointVerdict::Ok => None,
        crate::checkpoint::CheckpointVerdict::Violation { reason } => Some(format!(
            "checkpoint hygiene: {reason}. Call the `checkpoint` tool before editing a second \
             file"
        )),
    }
}

/// Record a task failure that carries an explanation.
///
/// Every early return in `run_worker` used to leave the run log silent, so a
/// dead worker looked identical to a worker that had simply not finished yet.
/// The retry ladder then reported "failed without outcome summary" to the next
/// rung, which re-ran the same work blind.
pub async fn record_failure(
    store: &tokio::sync::Mutex<RunStore>,
    spec: &WorkerSpec,
    agent_id: &str,
    summary: String,
) {
    let task_id = &spec.task.id;
    tracing::warn!(target: "pilot::worker", task = %task_id, "{summary}");
    record_attempt(
        store,
        spec,
        agent_id,
        TaskStatus::Failed,
        summary.clone(),
        &[],
    )
    .await;
    let mut guard = store.lock().await;
    let _ = guard
        .append(Event::TaskStatus {
            t: RunStore::now(),
            id: task_id.to_string(),
            status: TaskStatus::Failed,
            outcome: Some(TaskOutcome {
                summary,
                files_changed: Vec::new(),
            }),
        })
        .await;
    let _ = guard
        .append(Event::AgentStatus {
            t: RunStore::now(),
            agent: agent_id.to_string(),
            status: AgentStatus::Failed,
        })
        .await;
}

/// E5/R3 — record how this attempt ended, with the passing-test counts its
/// test-running checks reported (J15). Written before the attempt's final
/// `task.status`: the retry ladder reassigns on that status and kills this
/// attempt's task as it does, so anything written after it can be lost.
async fn record_attempt(
    store: &tokio::sync::Mutex<RunStore>,
    spec: &WorkerSpec,
    agent_id: &str,
    status: TaskStatus,
    summary: String,
    results: &[crate::acceptance::AcceptanceResult],
) {
    let tests = results
        .iter()
        .filter_map(|r| r.passed_tests.map(|n| (r.label.clone(), n)))
        .collect();
    let _ = store
        .lock()
        .await
        .append(Event::TaskAttempt {
            t: RunStore::now(),
            id: spec.task.id.clone(),
            agent: agent_id.to_string(),
            rung: spec.rung,
            model: spec.model.clone(),
            status,
            summary,
            tests,
        })
        .await;
}

/// One parsed line from the worker's stdout.
enum WorkerLine {
    AgentEvent(wingman_core::AgentEvent),
    WorkerStart {
        _model: String,
    },
    /// Carries both the outcome AND the acceptance results so the worker
    /// supervisor can gate the Review transition (E3) on green checks.
    TaskComplete {
        outcome: TaskOutcome,
        acceptance: Vec<crate::acceptance::AcceptanceResult>,
    },
    /// E9 — the worker's provider answered 429 / 529. Emitted by the worker
    /// shim for every such response, including ones the provider retried.
    RateLimited {
        status: u16,
        retry_after_secs: Option<u32>,
    },
    /// A Claude Code worker's subscription usage: the fullest limit window.
    SubscriptionUsage {
        utilization: f64,
        resets_at: Option<u64>,
    },
    Unknown,
}

/// Should the supervisor re-verify acceptance to salvage a worker that
/// exited without a terminal `task_complete`? Only when it exited cleanly
/// (a crash/error is a real failure, not a forgotten signal) and the task
/// actually declared checks worth re-running.
fn should_reverify(
    outcome_present: bool,
    process_ok: bool,
    declared: &[crate::model::Acceptance],
) -> bool {
    !outcome_present && process_ok && !declared.is_empty()
}

/// Explain a failure the worker never explained itself.
///
/// Three distinct situations, which used to collapse into two and produce
/// "acceptance checks failed: no acceptance checks defined" — a sentence that
/// contradicts itself and points away from the real failure.
pub fn failure_summary(
    results: &[crate::acceptance::AcceptanceResult],
    declared: &[crate::model::Acceptance],
    exit_code: Option<i32>,
    stop: Option<wingman_core::AgentStop>,
) -> String {
    let exit = exit_code.map_or_else(|| "abnormally".to_string(), |c| format!("with code {c}"));

    // Running out of turns is the one stop reason that looks like success
    // from the outside: the process exits 0 having quietly abandoned the job
    // mid-edit. Name it, because the fix is a config knob rather than a
    // retry — and a retry is what the ladder will otherwise do, four times.
    if matches!(stop, Some(wingman_core::AgentStop::MaxTurns)) {
        return if results.is_empty() {
            "worker ran out of turns before finishing (raise [pilot] worker_max_turns)".to_string()
        } else {
            format!(
                "worker ran out of turns before finishing (raise [pilot] worker_max_turns);                  acceptance at that point: {}",
                crate::acceptance::summarize(results)
            )
        };
    }

    if !results.is_empty() {
        // The checks ran and told us how they went.
        format!(
            "acceptance checks failed: {}",
            crate::acceptance::summarize(results)
        )
    } else if !declared.is_empty() {
        // Checks were declared but nothing came back: the worker stopped
        // before it got to them.
        format!(
            "worker exited {exit} without running its {} declared acceptance check(s)",
            declared.len()
        )
    } else {
        format!("worker exited {exit} without reporting completion")
    }
}

/// E3 status-gate function. Pure so the green/red transition is unit-testable.
///
/// Rules:
/// - Worker has to report `task_complete` (outcome.is_some()).
/// - Subprocess has to exit cleanly (status.success()).
/// - If the task declared acceptance checks, every result must be green.
///   No declared checks → vacuously green.
/// - Any failure routes to Failed so the retry watchdog (Phase 8.1) can
///   pick the task up.
pub fn compute_final_status(
    outcome: &Option<TaskOutcome>,
    process_ok: bool,
    declared: &[crate::model::Acceptance],
    results: &[crate::acceptance::AcceptanceResult],
) -> TaskStatus {
    if outcome.is_none() || !process_ok {
        return TaskStatus::Failed;
    }
    let acceptance_green = if declared.is_empty() {
        true
    } else {
        crate::acceptance::all_green(results)
    };
    if acceptance_green {
        TaskStatus::Review
    } else {
        TaskStatus::Failed
    }
}

fn parse_line(line: &str) -> WorkerLine {
    // `worker_start` and `task_complete` are flat JSON objects with an
    // `event` discriminator (emitted by the worker shim, not the agent
    // loop). Try those first; everything else routes through the
    // `AgentEvent` discriminator (`type`).
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
        if let Some(ev) = v.get("event").and_then(|x| x.as_str()) {
            return match ev {
                "worker_start" => WorkerLine::WorkerStart {
                    _model: v
                        .get("model")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                },
                "task_complete" => {
                    let summary = v
                        .get("summary")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let files_changed = v
                        .get("files_changed")
                        .and_then(|x| x.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|s| s.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    let acceptance: Vec<crate::acceptance::AcceptanceResult> = v
                        .get("acceptance_results")
                        .and_then(|x| serde_json::from_value(x.clone()).ok())
                        .unwrap_or_default();
                    WorkerLine::TaskComplete {
                        outcome: TaskOutcome {
                            summary,
                            files_changed,
                        },
                        acceptance,
                    }
                }
                "subscription_usage" => match v.get("utilization").and_then(|x| x.as_f64()) {
                    Some(utilization) => WorkerLine::SubscriptionUsage {
                        utilization,
                        resets_at: v.get("resets_at").and_then(|x| x.as_u64()),
                    },
                    None => WorkerLine::Unknown,
                },
                "rate_limited" => WorkerLine::RateLimited {
                    status: v.get("status").and_then(|x| x.as_u64()).unwrap_or(429) as u16,
                    retry_after_secs: v
                        .get("retry_after_secs")
                        .and_then(|x| x.as_u64())
                        .map(|n| n.min(u64::from(u32::MAX)) as u32),
                },
                _ => WorkerLine::Unknown,
            };
        }
        // AgentEvent has a `type` discriminator.
        if v.get("type").is_some() {
            if let Ok(ev) = serde_json::from_value::<wingman_core::AgentEvent>(v) {
                return WorkerLine::AgentEvent(ev);
            }
        }
    }
    WorkerLine::Unknown
}

/// Warn (once per distinct model id) that a model has no entry in the price
/// table, so its spend is counted as $0 and the `max_usd` cap can't protect
/// against a runaway run on that model. Empty model ids (local/unknown) are
/// skipped since they're expected to be unpriced.
fn warn_unpriced_model(model: &str) {
    use std::sync::Mutex;
    use std::sync::OnceLock;
    if model.is_empty() {
        return;
    }
    static WARNED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let set = WARNED.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    if let Ok(mut set) = set.lock() {
        if set.insert(model.to_string()) {
            tracing::warn!(
                target: "pilot::cost",
                model,
                "model has no price-table entry; its spend counts as $0, so the \
                 max_usd cap does NOT bound this model — add it to \
                 wingman_core::pricing or set a per-run agent limit"
            );
        }
    }
}

async fn forward_agent_event(
    store: &mut RunStore,
    task_id: &str,
    agent_id: &str,
    model: &str,
    event: &wingman_core::AgentEvent,
) {
    match event {
        wingman_core::AgentEvent::ToolStart { name, input, .. } => {
            // Pull the file this call touched (edit tools take `path`; a few
            // use `file_path`/`file`) so checkpoint hygiene can dedupe edits
            // by file rather than treating each call as a new file.
            let file = ["path", "file_path", "file"]
                .iter()
                .find_map(|k| input.get(k).and_then(|v| v.as_str()))
                .map(str::to_string);
            let _ = store
                .append(Event::TaskTool {
                    t: RunStore::now(),
                    id: task_id.to_string(),
                    agent: agent_id.to_string(),
                    tool: name.clone(),
                    input_hash: None,
                    file,
                    ok: true,
                })
                .await;
        }
        wingman_core::AgentEvent::ToolResult { is_error, .. } if *is_error => {
            // Tool result errors don't include the tool name; we already
            // logged the start. Future enhancement (E5 turn-gate) reads
            // this to gate the next turn.
        }
        wingman_core::AgentEvent::Usage { usage } => {
            // Price the usage so run totals reflect real spend — this is what
            // the max_usd cap and budget watchdog read. Unknown/local models
            // (no price table entry) fall back to 0.0.
            let usd = match wingman_core::pricing::price_for(model) {
                Some(p) => p.cost(usage),
                None => {
                    warn_unpriced_model(model);
                    0.0
                }
            };
            let _ = store
                .append(Event::AgentUsd {
                    t: RunStore::now(),
                    agent: agent_id.to_string(),
                    model: model.to_string(),
                    input_tokens: usage.input_tokens as u64,
                    output_tokens: usage.output_tokens as u64,
                    usd,
                })
                .await;
        }
        _ => {}
    }
}

/// Test seam: drive the post-spawn parse/forward loop against a fake stdout
/// stream, returning the final outcome and exit status the caller would
/// have observed. Used by the Phase 3 acceptance test without needing to
/// spawn a real subprocess.
#[cfg(test)]
pub async fn drive_stdout_for_test(
    store: &mut RunStore,
    task_id: &str,
    agent_id: &str,
    role: Role,
    model: &str,
    stdout: impl tokio::io::AsyncRead + Unpin,
    session_id: &str,
) -> Result<Option<TaskOutcome>, WorkerError> {
    let _ = store
        .append(Event::AgentSpawn {
            t: RunStore::now(),
            agent: agent_id.to_string(),
            role,
            pid: Some(0),
            session_id: Some(session_id.to_string()),
        })
        .await;
    let mut reader = BufReader::new(stdout).lines();
    let mut outcome: Option<TaskOutcome> = None;
    while let Some(line) = reader.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_line(line) {
            WorkerLine::AgentEvent(ev) => {
                forward_agent_event(store, task_id, agent_id, model, &ev).await;
            }
            WorkerLine::WorkerStart { .. } => {
                let _ = store
                    .append(Event::AgentStatus {
                        t: RunStore::now(),
                        agent: agent_id.to_string(),
                        status: AgentStatus::InProgress,
                    })
                    .await;
                let _ = store
                    .append(Event::TaskStatus {
                        t: RunStore::now(),
                        id: task_id.to_string(),
                        status: TaskStatus::InProgress,
                        outcome: None,
                    })
                    .await;
            }
            WorkerLine::TaskComplete { outcome: o, .. } => {
                outcome = Some(o);
            }
            WorkerLine::RateLimited { .. }
            | WorkerLine::SubscriptionUsage { .. }
            | WorkerLine::Unknown => {}
        }
    }
    let final_status = if outcome.is_some() {
        TaskStatus::Review
    } else {
        TaskStatus::Failed
    };
    let _ = store
        .append(Event::TaskStatus {
            t: RunStore::now(),
            id: task_id.to_string(),
            status: final_status,
            outcome: outcome.clone(),
        })
        .await;
    let _ = store
        .append(Event::AgentStatus {
            t: RunStore::now(),
            agent: agent_id.to_string(),
            status: if final_status == TaskStatus::Failed {
                AgentStatus::Failed
            } else {
                AgentStatus::Done
            },
        })
        .await;
    Ok(outcome)
}

/// `wingman` arguments for one worker, shared by the host spawn and the
/// sandbox script (which passes guest paths).
fn worker_args(
    spec: &WorkerSpec,
    task_file: &std::ffi::OsStr,
    worktree: &std::ffi::OsStr,
    tool_synthesis: Option<crate::approval::ApprovalTier>,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "--worker-mode".into(),
        "--task-file".into(),
        task_file.into(),
        "--role".into(),
        spec.role.as_str().into(),
        "--session-id".into(),
        spec.session_id.as_str().into(),
        "--worktree".into(),
        worktree.into(),
        // Signals headless to suppress TUI init. Headless `--print` needs a
        // prompt, but the worker-mode entry runs first, so it is never read.
        "--print".into(),
        "noop".into(),
        "--json".into(),
    ];
    // Forward the resolved model. The worker `cd`s into the worktree, which
    // does not contain the project's untracked `.wingman/config.toml`, so it
    // cannot rediscover `pilot.worker_model` on its own — without this the
    // child falls back to global config and dies with "no default_provider
    // configured", deadlocking every run. `--model` (env WINGMAN_MODEL) is
    // read as `opts.model_override` by worker-mode.
    if let Some(model) = &spec.model {
        args.extend(["--model".into(), model.into()]);
    }
    // Like the model, the tier that decides these lives in project config the
    // worker cannot see from inside its worktree.
    if spec.turn_rollback_after > 0 {
        args.extend([
            "--turn-rollback-after".into(),
            spec.turn_rollback_after.to_string().into(),
        ]);
    }
    if spec.checkpoint_hygiene {
        args.push("--checkpoint-hygiene".into());
    }
    // Decided here, not in the worker: the worker's config comes from inside
    // the worktree, so it sees neither `pilot run --tier` nor the project
    // config the trust decision is about.
    if let Some(tier) = tool_synthesis {
        args.extend(["--tool-synthesis".into(), tier.to_string().into()]);
    }
    args
}

fn write_task_file(task: &Task, worktree: &Path) -> Result<PathBuf, WorkerError> {
    // Put the task JSON inside the worktree's .wingman/ subdir so it's
    // visible to the worker without needing extra env vars.
    let dir = worktree.join(".wingman").join("pilot");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("task-{}.json", task.id));
    let body = serde_json::to_vec_pretty(task)?;
    std::fs::write(&path, body)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;

    /// The host spawn and the sandbox script share one argument list; the
    /// sandbox only swaps in guest paths.
    #[test]
    fn worker_args_carry_paths_and_model() {
        let spec = |model: Option<&str>| WorkerSpec {
            wingman_bin: PathBuf::from("wingman"),
            task: Task::new("t1", Role::Developer, "x"),
            role: Role::Developer,
            worktree: PathBuf::from("w"),
            session_id: "s1".into(),
            model: model.map(Into::into),
            timeout: Duration::from_secs(1),
            cmd_rx: None,
            rung: 0,
            turn_rollback_after: 0,
            checkpoint_hygiene: false,
            sandbox: None,
            tool_synthesis: None,
        };
        let strings = |args: Vec<std::ffi::OsString>| {
            args.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let args = |model| {
            strings(worker_args(
                &spec(model),
                "/work/.wingman/pilot/task-t1.json".as_ref(),
                "/work".as_ref(),
                None,
            ))
        };
        let with = args(Some("m1"));
        assert_eq!(
            &with[..3],
            [
                "--worker-mode",
                "--task-file",
                "/work/.wingman/pilot/task-t1.json"
            ]
        );
        let at = |flag: &str| &with[with.iter().position(|a| a == flag).unwrap() + 1];
        assert_eq!(at("--role"), "developer");
        assert_eq!(at("--session-id"), "s1");
        assert_eq!(at("--worktree"), "/work");
        assert_eq!(&with[with.len() - 2..], ["--model", "m1"]);
        assert!(!args(None).iter().any(|a| a == "--model"));
        assert!(!with.iter().any(|a| a == "--tool-synthesis"));
        assert!(!with.iter().any(|a| a == "--checkpoint-hygiene"));

        let synth = strings(worker_args(
            &spec(None),
            "t".as_ref(),
            "w".as_ref(),
            Some(crate::approval::ApprovalTier::Hard),
        ));
        assert_eq!(&synth[synth.len() - 2..], ["--tool-synthesis", "hard-gate"]);

        let mut gated = spec(None);
        gated.turn_rollback_after = 3;
        gated.checkpoint_hygiene = true;
        let gated = strings(worker_args(&gated, "t".as_ref(), "w".as_ref(), None));
        assert_eq!(
            &gated[gated.len() - 3..],
            ["--turn-rollback-after", "3", "--checkpoint-hygiene"]
        );
    }

    /// Phase 3 acceptance (plan.md line 638): a single task executes
    /// end-to-end, events stream into tasks.jsonl, run exits cleanly.
    ///
    /// We can't drive a real LLM in unit tests, so we simulate the worker
    /// subprocess: a stream of NDJSON lines exactly like a real worker would
    /// emit (worker_start, tool_start, tool_result, text_delta, usage,
    /// task_complete, stop). The parser + forwarder pipeline runs over the
    /// canned stream and we assert the resulting tasks.jsonl contents.
    #[tokio::test]
    async fn worker_pipeline_round_trip() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let mut store = RunStore::create(dir.path(), "r1", "g", "abc", "wingman/auto/r1")
            .await
            .unwrap();

        // Pre-create the task via task.create so the forwarder can mutate it.
        store
            .append(Event::TaskCreate {
                t: RunStore::now(),
                id: "t1".into(),
                role: Role::Developer,
                title: "Add --version-only flag".into(),
                goal: "wire fast-exit flag".into(),
                deps: vec![],
                writes: vec!["crates/wingman-cli/src/args.rs".into()],
                acceptance: vec![],
                reversibility: Default::default(),
                reversibility_reason: None,
            })
            .await
            .unwrap();

        // Canned worker stdout. Each line is exactly what a real worker
        // would print to stdout in `--worker-mode --print --json`.
        let canned = concat!(
            r#"{"event":"worker_start","task_id":"t1","role":"developer","session_id":"sess-1","model":"claude-haiku-4-5","provider":"anthropic"}"#,
            "\n",
            r#"{"type":"tool_start","id":"call-1","name":"edit_file","input":{"path":"crates/wingman-cli/src/args.rs"}}"#,
            "\n",
            r#"{"type":"tool_result","id":"call-1","output":"ok","is_error":false}"#,
            "\n",
            r#"{"type":"usage","usage":{"input_tokens":1200,"output_tokens":300,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#,
            "\n",
            r#"{"event":"task_complete","summary":"Added --version-only flag to args.rs.","files_changed":["crates/wingman-cli/src/args.rs"]}"#,
            "\n",
            r#"{"type":"stop","reason":"end_turn"}"#,
            "\n",
        );
        let cursor = std::io::Cursor::new(canned.as_bytes());

        let outcome = drive_stdout_for_test(
            &mut store,
            "t1",
            "agent-1",
            Role::Developer,
            "claude-haiku-4-5",
            cursor,
            "sess-1",
        )
        .await
        .unwrap();

        // Outcome captured from the task_complete marker.
        let outcome = outcome.expect("worker reported task_complete");
        assert!(outcome.summary.contains("--version-only"));
        assert_eq!(
            outcome.files_changed,
            vec!["crates/wingman-cli/src/args.rs"]
        );

        // The task moved through in_progress → review.
        let task = store.state().task("t1").expect("task t1 in state");
        assert_eq!(task.status, TaskStatus::Review);
        assert!(task.outcome.is_some());
        assert_eq!(
            task.outcome.as_ref().unwrap().files_changed,
            vec!["crates/wingman-cli/src/args.rs"]
        );

        // The agent moved through idle → in_progress → done.
        let agent = store.state().agent("agent-1").expect("agent registered");
        assert_eq!(agent.status, AgentStatus::Done);
        assert_eq!(agent.session_id.as_deref(), Some("sess-1"));

        // Usage was forwarded into the run totals, and priced: haiku-4-5 is
        // $1/Mtok in + $5/Mtok out → 1200*1e-6 + 300*5e-6 = $0.0027. This is
        // what the max_usd cap reads.
        assert_eq!(store.state().totals.tokens_in, 1200);
        assert_eq!(store.state().totals.tokens_out, 300);
        assert!(
            (store.state().totals.usd - 0.0027).abs() < 1e-9,
            "expected priced usd ~0.0027, got {}",
            store.state().totals.usd
        );

        // Tool invocation was recorded for the live log. We can't read
        // tasks.jsonl back through the snapshot (task.tool intentionally
        // doesn't mutate state), so just verify the log contains the
        // expected event.
        let log = std::fs::read_to_string(store.log_path()).unwrap();
        assert!(
            log.contains(r#""ev":"task.tool""#) && log.contains(r#""tool":"edit_file""#),
            "tasks.jsonl missing task.tool event:\n{log}"
        );
        // And task_complete propagated via the final task.status event.
        assert!(log.contains(r#""status":"review""#));
    }

    #[test]
    fn parse_line_recognises_worker_start_and_task_complete() {
        let line = r#"{"event":"task_complete","summary":"done","files_changed":["a.rs"]}"#;
        match parse_line(line) {
            WorkerLine::TaskComplete {
                outcome: o,
                acceptance,
            } => {
                assert_eq!(o.summary, "done");
                assert_eq!(o.files_changed, vec!["a.rs"]);
                assert!(
                    acceptance.is_empty(),
                    "no acceptance_results in this payload"
                );
            }
            _ => panic!("expected TaskComplete"),
        }

        let line = r#"{"event":"worker_start","model":"x"}"#;
        matches!(parse_line(line), WorkerLine::WorkerStart { .. });
    }

    #[test]
    fn compute_final_status_routes_correctly_for_each_signal() {
        use crate::acceptance::AcceptanceResult;
        use crate::model::Acceptance;

        let ok_outcome = Some(TaskOutcome {
            summary: "done".into(),
            files_changed: vec![],
        });

        // No outcome → Failed.
        assert_eq!(
            compute_final_status(&None, true, &[], &[]),
            TaskStatus::Failed
        );
        // Outcome + non-zero exit → Failed.
        assert_eq!(
            compute_final_status(&ok_outcome, false, &[], &[]),
            TaskStatus::Failed
        );
        // Outcome + zero exit + no declared checks → Review.
        assert_eq!(
            compute_final_status(&ok_outcome, true, &[], &[]),
            TaskStatus::Review
        );
        // Declared checks, all green → Review.
        let declared = vec![Acceptance::Shell { cmd: "true".into() }];
        let green = vec![AcceptanceResult::ok("shell: true", "")];
        assert_eq!(
            compute_final_status(&ok_outcome, true, &declared, &green),
            TaskStatus::Review
        );
        // Declared checks, one red → Failed (E3 gate).
        let red = vec![
            AcceptanceResult::ok("shell: true", ""),
            AcceptanceResult::fail("shell: cargo test", "exit 1"),
        ];
        assert_eq!(
            compute_final_status(&ok_outcome, true, &declared, &red),
            TaskStatus::Failed
        );
        // Declared checks, results empty → Failed (worker fabricated /
        // forgot to call run_acceptance).
        assert_eq!(
            compute_final_status(&ok_outcome, true, &declared, &[]),
            TaskStatus::Failed
        );
    }

    #[test]
    fn should_reverify_only_salvages_clean_exits_with_checks() {
        use crate::model::Acceptance;
        let checks = vec![Acceptance::Shell { cmd: "true".into() }];
        // The salvage case: no terminal outcome, clean exit, checks declared.
        assert!(should_reverify(false, true, &checks));
        // Worker already reported completion → nothing to salvage.
        assert!(!should_reverify(true, true, &checks));
        // Non-zero exit is a real crash, not a forgotten signal → no salvage.
        assert!(!should_reverify(false, false, &checks));
        // No declared checks → nothing to re-verify against.
        assert!(!should_reverify(false, true, &[]));
    }

    #[test]
    fn parse_line_extracts_acceptance_results() {
        let line = r#"{
            "event":"task_complete",
            "summary":"done",
            "files_changed":["a.rs"],
            "acceptance_results":[
                {"label":"shell: cargo check","ok":true,"output":""},
                {"label":"grep: foo in a.rs","ok":false,"output":"pattern foo not found in a.rs"}
            ]
        }"#;
        match parse_line(line) {
            WorkerLine::TaskComplete {
                outcome,
                acceptance,
            } => {
                assert_eq!(outcome.summary, "done");
                assert_eq!(acceptance.len(), 2);
                assert!(acceptance[0].ok);
                assert!(!acceptance[1].ok);
                assert!(acceptance[1].output.contains("not found"));
            }
            other => panic!("expected TaskComplete with acceptance, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_recognises_rate_limited() {
        match parse_line(r#"{"event":"rate_limited","status":529,"retry_after_secs":12}"#) {
            WorkerLine::RateLimited {
                status,
                retry_after_secs,
            } => {
                assert_eq!(status, 529);
                assert_eq!(retry_after_secs, Some(12));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        assert!(matches!(
            parse_line(r#"{"event":"rate_limited","status":429}"#),
            WorkerLine::RateLimited {
                retry_after_secs: None,
                ..
            }
        ));
    }

    #[test]
    fn parse_line_recognises_subscription_usage_and_state_keeps_the_latest() {
        let line = r#"{"event":"subscription_usage","utilization":0.83,"resets_at":1789668000}"#;
        let WorkerLine::SubscriptionUsage {
            utilization,
            resets_at,
        } = parse_line(line)
        else {
            panic!("expected SubscriptionUsage");
        };
        assert_eq!((utilization, resets_at), (0.83, Some(1789668000)));
        assert!(matches!(
            parse_line(r#"{"event":"subscription_usage"}"#),
            WorkerLine::Unknown
        ));

        let mut state = crate::model::RunState::new("r", "g", "b", "i");
        crate::model::apply(
            &mut state,
            &Event::SubscriptionUsage {
                t: RunStore::now(),
                agent: "a1".into(),
                utilization,
                resets_at,
            },
        );
        assert_eq!(state.subscription.map(|s| s.utilization), Some(0.83));
    }

    #[test]
    fn parse_line_handles_agent_event() {
        let line = r#"{"type":"text_delta","text":"hello"}"#;
        match parse_line(line) {
            WorkerLine::AgentEvent(wingman_core::AgentEvent::TextDelta { text }) => {
                assert_eq!(text, "hello");
            }
            other => panic!("expected AgentEvent::TextDelta, got {other:?}"),
        }
    }

    impl std::fmt::Debug for WorkerLine {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                WorkerLine::AgentEvent(_) => write!(f, "AgentEvent"),
                WorkerLine::WorkerStart { .. } => write!(f, "WorkerStart"),
                WorkerLine::TaskComplete { .. } => write!(f, "TaskComplete"),
                WorkerLine::RateLimited { .. } => write!(f, "RateLimited"),
                WorkerLine::SubscriptionUsage { .. } => write!(f, "SubscriptionUsage"),
                WorkerLine::Unknown => write!(f, "Unknown"),
            }
        }
    }

    /// A worker that dies without reporting used to leave the run log silent:
    /// the task sat at `in_progress` with a dead process until the manager
    /// reassigned it, and the next rung was handed "failed without outcome
    /// summary". That is what made a $1.15 pilot run undiagnosable.
    #[tokio::test]
    async fn record_failure_writes_a_readable_reason() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::create(dir.path(), "r1", "goal", "base", "branch")
            .await
            .unwrap();
        let store = tokio::sync::Mutex::new(store);

        let spec = WorkerSpec {
            wingman_bin: PathBuf::from("wingman"),
            task: Task::new("t1", Role::Developer, "x"),
            role: Role::Developer,
            worktree: dir.path().to_path_buf(),
            session_id: "s".into(),
            model: Some("haiku".into()),
            timeout: Duration::from_secs(1800),
            cmd_rx: None,
            rung: 2,
            turn_rollback_after: 0,
            checkpoint_hygiene: false,
            sandbox: None,
            tool_synthesis: None,
        };
        record_failure(&store, &spec, "agent-0001", "worker exceeded 1800s".into()).await;

        let guard = store.lock().await;
        let task = guard.state().task("t1");
        // The task does not exist in this bare store, but the event is on
        // disk either way -- assert on the log, which is the source of truth.
        assert!(task.is_none());
        let log = std::fs::read_to_string(guard.log_path()).unwrap();
        assert!(
            log.contains("worker exceeded 1800s"),
            "reason must be recorded:
{log}"
        );
        assert!(log.contains("\"status\":\"failed\""));
        // The ladder telemetry lands first, so the reassign the failed status
        // triggers cannot cut it off.
        let attempt = log
            .find("\"ev\":\"task.attempt\"")
            .expect("attempt recorded");
        let status = log.find("\"ev\":\"task.status\"").expect("status recorded");
        assert!(attempt < status, "{log}");
        assert!(log.contains("\"rung\":2") && log.contains("\"model\":\"haiku\""));
    }

    /// E11: multi-file work reaches Review only when this attempt checkpointed.
    #[tokio::test]
    async fn checkpoint_violation_reads_the_attempts_tool_calls() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::create(dir.path(), "r1", "goal", "base", "branch")
            .await
            .unwrap();
        let store = tokio::sync::Mutex::new(store);
        let tool = |name: &str, file: &str| Event::TaskTool {
            t: RunStore::now(),
            id: "t1".into(),
            agent: "agent-0001".into(),
            tool: name.into(),
            input_hash: None,
            file: Some(file.into()),
            ok: true,
        };
        for ev in [tool("edit_file", "a.rs"), tool("write_file", "b.rs")] {
            store.lock().await.append(ev).await.unwrap();
        }
        let reason = checkpoint_violation(&store, "t1").await.expect("violation");
        assert!(
            reason.contains("rule 1") && reason.contains("`checkpoint` tool"),
            "{reason}"
        );

        // The retry checkpoints first and passes.
        store
            .lock()
            .await
            .append(Event::TaskAssign {
                t: RunStore::now(),
                id: "t1".into(),
                agent: "agent-0002".into(),
                worktree: "wt".into(),
            })
            .await
            .unwrap();
        for ev in [
            tool("checkpoint", ""),
            tool("edit_file", "a.rs"),
            tool("write_file", "b.rs"),
        ] {
            store.lock().await.append(ev).await.unwrap();
        }
        assert_eq!(checkpoint_violation(&store, "t1").await, None);
    }

    /// Observed on run 2026-08-21-1920-xoyw4q: t1 declared three acceptance
    /// checks, the worker stopped before running any of them, and the record
    /// read "acceptance checks failed: no acceptance checks defined" — which
    /// contradicts itself and points away from the real failure.
    #[test]
    fn failure_summary_separates_unrun_checks_from_failed_ones() {
        use crate::acceptance::AcceptanceResult;
        use crate::model::Acceptance;

        let declared = [
            Acceptance::Shell {
                cmd: "cargo check".into(),
            },
            Acceptance::Grep {
                pattern: "version_only".into(),
                path: "src/main.rs".into(),
            },
        ];

        // Declared, never run: name that, not a check failure.
        let s = failure_summary(&[], &declared, Some(0), None);
        assert_eq!(
            s,
            "worker exited with code 0 without running its 2 declared acceptance check(s)"
        );
        assert!(!s.contains("no acceptance checks defined"));

        // Ran and failed: report the verdict.
        let results = [AcceptanceResult {
            label: "grep version_only".into(),
            ok: false,
            output: "no match".into(),
            passed_tests: None,
        }];
        let s = failure_summary(&results, &declared, Some(1), None);
        assert!(s.starts_with("acceptance checks failed:"), "{s}");
        assert!(s.contains("grep version_only"), "carries the detail: {s}");

        // Nothing declared at all: the plain case.
        assert_eq!(
            failure_summary(&[], &[], None, None),
            "worker exited abnormally without reporting completion"
        );
    }

    /// Observed on runs 2026-08-21-1729 and -1920: workers stopped at exactly
    /// the interactive 16-turn default, exited 0, and the record could only
    /// say "without reporting completion" -- which reads as a model failure
    /// and sends the retry ladder round again. Four times, on the second run.
    #[test]
    fn running_out_of_turns_says_so() {
        use wingman_core::AgentStop;
        let declared = [crate::model::Acceptance::Shell {
            cmd: "cargo check".into(),
        }];

        let s = failure_summary(&[], &declared, Some(0), Some(AgentStop::MaxTurns));
        assert!(s.contains("ran out of turns"), "{s}");
        assert!(
            s.contains("worker_max_turns"),
            "must name the knob that fixes it: {s}"
        );

        // A different stop reason keeps the ordinary wording.
        let s = failure_summary(&[], &declared, Some(0), Some(AgentStop::EndTurn));
        assert!(s.contains("without running its 1 declared"), "{s}");
    }

    #[test]
    fn a_failed_gate_without_an_outcome_still_explains_itself() {
        use crate::acceptance::AcceptanceResult;
        // Mirrors the synthesis in `run_worker`: a worker that fails the E3
        // gate without calling `task_complete` has no outcome of its own, so
        // the acceptance verdict has to become one.
        let results = vec![AcceptanceResult {
            label: "grep version_only in src/args.rs".into(),
            ok: false,
            output: "no match".into(),
            passed_tests: None,
        }];
        let summary = format!(
            "acceptance checks failed: {}",
            crate::acceptance::summarize(&results)
        );
        assert!(summary.contains("acceptance checks failed"));
        assert!(
            !summary.trim_end_matches(':').ends_with("failed"),
            "the summary must carry the per-check detail, not just a label"
        );
    }
}
