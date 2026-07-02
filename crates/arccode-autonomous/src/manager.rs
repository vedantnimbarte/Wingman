//! Manager agent loop.
//!
//! The manager is an in-process [`arccode_core::AgentLoop`] running the
//! configured `default_model`. Its restricted tool registry exposes only:
//!
//! - `add_task`, `assign_task`, `reassign_task`, `finalize_task`,
//!   `abort_task`, `message_agent` (the manager-only tools)
//! - `list_dir`, `read_file`, `grep_tool` (read-only inspection inherited
//!   from `arccode-tools::builtin`)
//!
//! The manager doesn't write files directly — every mutation flows through
//! the [`crate::orchestrator::OrchestratorHandle`]. That keeps the JSONL
//! log coherent and lets the dashboard subscribe to a single broadcast
//! stream.
//!
//! Phase 4 scope: one-shot `run_tick` that asks the manager to look at the
//! current state and pick a next move. A long-running loop that wakes on
//! every state transition lands in Phase 7.5 (E10) once IPC matures.

use std::sync::Arc;

use arccode_core::{AgentConfig, AgentEvent, AgentLoop, AgentStop, Provider};
use arccode_tools::{builtin, ToolCtx, ToolRegistry};
use futures::StreamExt;
use thiserror::Error;

use crate::model::{RunState, TaskStatus};
use crate::orchestrator::{OrchestratorError, OrchestratorHandle};
use crate::role::load_manager_prompt;

#[derive(Debug, Error)]
pub enum ManagerError {
    #[error("agent: {0}")]
    Agent(String),
    #[error("orchestrator: {0}")]
    Orchestrator(#[from] OrchestratorError),
    #[error("dependency deadlock — the task graph cannot make progress: {0}")]
    Deadlock(String),
}

/// Build the restricted tool registry the manager runs against.
///
/// `cwd` and `project_root` shape the [`ToolCtx`] for the read-only tools
/// (`list_dir`, `read_file`, `grep_tool`). The manager runs in read-only
/// permission mode — its writes only happen through its orchestrator
/// commands, never via filesystem tools.
pub fn build_manager_registry(
    handle: OrchestratorHandle,
    cwd: std::path::PathBuf,
    project_root: std::path::PathBuf,
) -> Arc<ToolRegistry> {
    let ctx = ToolCtx::new_with_config(
        arccode_config::PermissionMode::ReadOnly,
        cwd,
        project_root,
        Vec::new(),
    );
    let mut reg = ToolRegistry::new(ctx);
    // Read-only inspection.
    reg.register(builtin::ListDir);
    reg.register(builtin::ReadFile);
    reg.register(builtin::Grep);
    // Orchestration.
    reg.register(crate::tools::AddTask::new(handle.clone()));
    reg.register(crate::tools::AssignTask::new(handle.clone()));
    reg.register(crate::tools::ReassignTask::new(handle.clone()));
    reg.register(crate::tools::FinalizeTask::new(handle.clone()));
    reg.register(crate::tools::AbortTask::new(handle.clone()));
    reg.register(crate::tools::MessageAgent::new(handle));
    Arc::new(reg)
}

/// Build the manager [`AgentLoop`].
pub fn build_manager(
    provider: Arc<dyn Provider>,
    model: String,
    registry: Arc<ToolRegistry>,
    extra_system: Option<String>,
) -> AgentLoop {
    let mut system = load_manager_prompt();
    if let Some(extra) = extra_system {
        system.push_str("\n\n");
        system.push_str(&extra);
    }
    let cfg = AgentConfig {
        model,
        system: Some(system),
        max_turns: 32,
        ..Default::default()
    };
    AgentLoop::new(provider, registry, cfg)
}

/// Render the current run state into a status block the manager can read at
/// the start of each tick. Compact by design — the manager cares about
/// task statuses and dep edges, not arbitrary metadata.
pub fn render_state_block(state: &RunState) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "# Current run state\n");
    let _ = writeln!(s, "- run_id: {}", state.run_id);
    let _ = writeln!(s, "- goal: {}", state.goal);
    let _ = writeln!(
        s,
        "- totals: usd={:.2} tokens_in={} tokens_out={}",
        state.totals.usd, state.totals.tokens_in, state.totals.tokens_out
    );
    let _ = writeln!(s, "\n## Tasks");
    for t in &state.tasks {
        let deps = if t.deps.is_empty() {
            "—".to_string()
        } else {
            t.deps.join(", ")
        };
        let _ = writeln!(
            s,
            "- {} [{}] {:?} (deps: {deps}){}",
            t.id,
            t.role.as_str(),
            t.status,
            t.agent
                .as_deref()
                .map(|a| format!(" agent={a}"))
                .unwrap_or_default(),
        );
        if !t.title.is_empty() {
            let _ = writeln!(s, "    title: {}", t.title);
        }
    }
    s
}

/// Run one manager tick: build a user prompt from the current state, drive
/// the agent until it ends its turn, return how it stopped.
///
/// The manager's job in one tick is to make as many scheduling decisions
/// as it can. With `max_turns = 32` and the orchestrator processing
/// commands serially, a 3-task plan typically resolves in ~3 ticks.
pub async fn run_tick(agent: &mut AgentLoop, prompt: String) -> Result<AgentStop, ManagerError> {
    let mut stream = agent.run(prompt);
    let mut last_stop = AgentStop::EndTurn;
    while let Some(event) = stream.next().await {
        match event {
            AgentEvent::Stop { reason } => {
                last_stop = reason;
                break;
            }
            AgentEvent::Error { message } => {
                return Err(ManagerError::Agent(message));
            }
            _ => {}
        }
    }
    Ok(last_stop)
}

/// Drive the manager to completion: tick repeatedly until every task is
/// terminal (Done or Failed). Each tick gets a freshly-rendered state
/// block so the model sees the latest picture.
///
/// `max_ticks` is a safety belt — if the manager is looping fruitlessly
/// we bail out rather than burn budget forever. Real cost limits come
/// from [`crate::orchestrator::OrchestratorConfig`].
pub async fn drive_to_completion(
    agent: &mut AgentLoop,
    handle: &OrchestratorHandle,
    max_ticks: usize,
) -> Result<(), ManagerError> {
    // Fingerprint of the previous tick's task statuses. A dependency
    // deadlock is only declared when the picture is *unchanged* from the
    // prior tick (so the manager already had a full turn to break it — e.g.
    // by adding a task or finalizing a review) and still cannot progress.
    let mut prev_fingerprint: Option<String> = None;
    for tick in 0..max_ticks {
        let state = handle.snapshot().await?;
        if state.tasks.iter().all(|t| t.status.is_terminal()) {
            tracing::info!(target: "pilot::manager", tick, "all tasks terminal — exiting drive loop");
            return Ok(());
        }
        // Fail fast on a wedged graph instead of spinning fruitlessly (and
        // burning LLM budget) until `max_ticks`. A cycle or a dep on a
        // failed/blocked/missing task leaves tasks that can never become
        // eligible; catching it here turns a silent stall into a clear error.
        if let Some(reason) = dag_stall_reason(&state) {
            let fingerprint = state_fingerprint(&state);
            if prev_fingerprint.as_deref() == Some(fingerprint.as_str()) {
                tracing::warn!(target: "pilot::manager", tick, %reason, "dependency deadlock — aborting drive loop");
                return Err(ManagerError::Deadlock(reason));
            }
            // First observation: give the manager this tick to try to break
            // it, then re-check next tick against this fingerprint.
            prev_fingerprint = Some(fingerprint);
        } else {
            prev_fingerprint = Some(state_fingerprint(&state));
        }
        let prompt = format!(
            "{state_block}\n\n\
             Take the next scheduling step. If a task is in `review` and acceptance \
             was green, call finalize_task. If a task is `todo` (or `pending` whose \
             deps are now `done`) and capacity allows, call assign_task. If a task \
             is `failed`, decide between reassign_task (rung 2), splitting it via \
             add_task (rung 3), or abort_task. If nothing is actionable right now, \
             reply with one line ('waiting') and end your turn.",
            state_block = render_state_block(&state),
        );
        run_tick(agent, prompt).await?;
    }
    Err(ManagerError::Agent(format!(
        "drive_to_completion exceeded max_ticks={max_ticks} without converging"
    )))
}

/// Convenience: are all the tasks in `state` in a terminal state?
pub fn run_is_done(state: &RunState) -> bool {
    !state.tasks.is_empty() && state.tasks.iter().all(|t| t.status.is_terminal())
}

/// A compact, order-stable fingerprint of the run's task statuses. Used by
/// the drive loop to tell whether a tick changed anything.
fn state_fingerprint(state: &RunState) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(state.tasks.len() * 12);
    for t in &state.tasks {
        let _ = write!(s, "{}={:?};", t.id, t.status);
    }
    s
}

/// If the DAG can no longer make progress toward completion, return a
/// human-readable reason naming the stuck tasks and why; otherwise `None`.
///
/// The run can still progress while any not-yet-`Done` task is *actionable*:
/// - `InProgress` — a worker is running it,
/// - `Review` — the manager can finalize it,
/// - `Pending`/`Todo` whose deps are all `Done` — it can be assigned now.
///
/// When none of the unfinished tasks are actionable, the graph is wedged —
/// typically a dependency cycle, or a dep on a task that failed, blocked, or
/// never existed (so it will never reach `Done`). `Failed` on its own is
/// terminal and handled by the caller's all-terminal check; it only shows up
/// here when another task is stuck waiting on it.
fn dag_stall_reason(state: &RunState) -> Option<String> {
    use TaskStatus::*;
    let is_done = |id: &str| {
        state
            .task(id)
            .map(|t| t.status == Done)
            .unwrap_or(false)
    };
    let unfinished: Vec<&crate::model::Task> =
        state.tasks.iter().filter(|t| t.status != Done).collect();
    if unfinished.is_empty() {
        return None;
    }
    let can_progress = state.tasks.iter().any(|t| match t.status {
        InProgress | Review => true,
        Pending | Todo => t.deps.iter().all(|d| is_done(d)),
        Done | Failed | Blocked => false,
    });
    if can_progress {
        return None;
    }
    let mut reasons: Vec<String> = Vec::new();
    for t in &unfinished {
        match t.status {
            Failed => reasons.push(format!("{} failed with no retry path", t.id)),
            Blocked => reasons.push(format!("{} is blocked (retry ladder exhausted)", t.id)),
            Pending | Todo => {
                let unmet: Vec<String> = t
                    .deps
                    .iter()
                    .filter(|d| !is_done(d))
                    .map(|d| match state.task(d) {
                        None => format!("{d} (missing)"),
                        Some(dep) => format!("{d} ({:?})", dep.status),
                    })
                    .collect();
                reasons.push(format!("{} waits on [{}]", t.id, unmet.join(", ")));
            }
            InProgress | Review | Done => {}
        }
    }
    Some(reasons.join("; "))
}

/// Convenience: did every task end up `done` (vs `failed`)?
pub fn run_succeeded(state: &RunState) -> bool {
    !state.tasks.is_empty() && state.tasks.iter().all(|t| t.status == TaskStatus::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Role, Task};

    fn task(id: &str, status: TaskStatus, deps: &[&str]) -> Task {
        let mut t = Task::new(id, Role::Developer, id);
        t.status = status;
        t.deps = deps.iter().map(|s| s.to_string()).collect();
        t
    }

    fn state(tasks: Vec<Task>) -> RunState {
        let mut s = RunState::new("r", "goal", "base", "branch");
        s.tasks = tasks;
        s
    }

    #[test]
    fn no_stall_when_a_task_is_eligible() {
        // t1 has no deps → assignable now; the run can progress.
        let s = state(vec![
            task("t1", TaskStatus::Pending, &[]),
            task("t2", TaskStatus::Pending, &["t1"]),
        ]);
        assert!(dag_stall_reason(&s).is_none());
    }

    #[test]
    fn no_stall_while_a_worker_runs() {
        let s = state(vec![
            task("t1", TaskStatus::InProgress, &[]),
            task("t2", TaskStatus::Pending, &["t1"]),
        ]);
        assert!(dag_stall_reason(&s).is_none());
    }

    #[test]
    fn no_stall_while_a_task_awaits_finalize() {
        // A Review task is actionable — the manager can finalize it.
        let s = state(vec![
            task("t1", TaskStatus::Review, &[]),
            task("t2", TaskStatus::Pending, &["t1"]),
        ]);
        assert!(dag_stall_reason(&s).is_none());
    }

    #[test]
    fn no_stall_when_all_done() {
        let s = state(vec![task("t1", TaskStatus::Done, &[])]);
        assert!(dag_stall_reason(&s).is_none());
    }

    #[test]
    fn stall_detected_on_dependency_cycle() {
        // t1 ⇄ t2: neither can ever have its deps Done, nothing is running.
        let s = state(vec![
            task("t1", TaskStatus::Pending, &["t2"]),
            task("t2", TaskStatus::Pending, &["t1"]),
        ]);
        let reason = dag_stall_reason(&s).expect("a cycle must be flagged as a stall");
        assert!(reason.contains("t1") && reason.contains("t2"), "got: {reason}");
    }

    #[test]
    fn stall_when_pending_waits_on_a_failed_dep() {
        // t1 failed and won't be retried; t2 can never become eligible.
        let s = state(vec![
            task("t1", TaskStatus::Failed, &[]),
            task("t2", TaskStatus::Pending, &["t1"]),
        ]);
        let reason = dag_stall_reason(&s).expect("waiting on a failed dep is a stall");
        assert!(reason.contains("t2"), "got: {reason}");
    }

    #[test]
    fn stall_when_only_blocked_tasks_remain() {
        // Blocked isn't terminal (is_terminal = Done|Failed), so without this
        // detector the drive loop would spin to max_ticks.
        let s = state(vec![task("t1", TaskStatus::Blocked, &[])]);
        assert!(dag_stall_reason(&s).is_some());
    }

    #[test]
    fn fingerprint_tracks_status_changes() {
        let a = state(vec![task("t1", TaskStatus::Pending, &[])]);
        let b = state(vec![task("t1", TaskStatus::InProgress, &[])]);
        assert_ne!(state_fingerprint(&a), state_fingerprint(&b));
        let c = state(vec![task("t1", TaskStatus::Pending, &[])]);
        assert_eq!(state_fingerprint(&a), state_fingerprint(&c));
    }
}
