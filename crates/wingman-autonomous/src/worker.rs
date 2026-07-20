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

    let mut sc = SupervisedCommand::new(&spec.wingman_bin);
    sc.command_mut()
        .arg("--worker-mode")
        .arg("--task-file")
        .arg(&task_path)
        .arg("--role")
        .arg(spec.role.as_str())
        .arg("--session-id")
        .arg(&spec.session_id)
        .arg("--worktree")
        .arg(&spec.worktree)
        .arg("--print") // signal headless to suppress TUI init
        .arg("noop") // headless --print needs a prompt; the worker-mode
        // entry runs before headless is invoked, so the
        // value is never read
        .arg("--json")
        .current_dir(&spec.worktree);

    // Forward the resolved model. The worker `cd`s into the worktree, which
    // does not contain the project's untracked `.wingman/config.toml`, so it
    // cannot rediscover `pilot.worker_model` on its own — without this the
    // child falls back to global config and dies with "no default_provider
    // configured", deadlocking every run. `--model` (env WINGMAN_MODEL) is
    // read as `opts.model_override` by worker-mode.
    if let Some(model) = &spec.model {
        sc.command_mut().arg("--model").arg(model);
    }

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
                WorkerLine::Unknown => {
                    tracing::debug!(target: "pilot::worker", "unrecognised worker line: {line}");
                }
            }
        }
        Ok::<
            (
                Option<TaskOutcome>,
                Vec<crate::acceptance::AcceptanceResult>,
            ),
            WorkerError,
        >((outcome, acceptance))
    };

    let (outcome, acceptance) = match timeout(spec.timeout, parse_loop).await {
        Ok(r) => r?,
        Err(_) => {
            supervisor.terminate(Duration::from_secs(2)).await.ok();
            return Err(WorkerError::Timeout(spec.timeout));
        }
    };

    let status = child.wait().await?;
    let exit_code = status.code();

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
    let (outcome, acceptance) = if should_reverify(
        outcome.is_some(),
        status.success(),
        &spec.task.acceptance,
    ) {
        let verified =
            crate::acceptance::run_acceptance_checks(&spec.task.acceptance, &spec.worktree);
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
            (outcome, acceptance)
        }
    } else {
        (outcome, acceptance)
    };

    // E3 gate: if the task declared acceptance checks, the worker MUST
    // have returned green results in order to move to Review. Otherwise
    // the task lands in Failed for the retry watchdog to pick up.
    let final_status = compute_final_status(
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

    let _ = store
        .lock()
        .await
        .append(Event::TaskStatus {
            t: RunStore::now(),
            id: spec.task.id.clone(),
            status: final_status,
            outcome: outcome.clone(),
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
        outcome,
        exit_code,
    })
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
            WorkerLine::Unknown => {}
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
                WorkerLine::Unknown => write!(f, "Unknown"),
            }
        }
    }
}
