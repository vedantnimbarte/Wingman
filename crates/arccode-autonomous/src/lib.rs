//! Pilot mode (formerly "autonomous mode") for arccode.
//!
//! See `plan.md` at the workspace root for the full design. M1 ships only
//! the foundation: data model + [`store::RunStore`]. Orchestrator, planner,
//! manager agent loop, worker subprocess supervisor, worktree merge, PR
//! creation and TUI dashboard land in later phases.

pub mod acceptance;
pub mod approval;
pub mod automerge;
pub mod checkpoint;
pub mod child_process;
pub mod concurrency;
pub mod control;
pub mod critic;
pub mod daemon;
pub mod dashboard;
pub mod escalation;
pub mod estimate;
pub mod eval;
pub mod feedback;
pub mod grounding;
pub mod handoff;
pub mod intake;
pub mod interject;
pub mod ipc;
pub mod knowledge;
pub mod learning;
pub mod manager;
pub mod model;
pub mod names;
pub mod notify;
pub mod orchestrator;
pub mod pipeline;
pub mod planner;
pub mod pr;
pub mod provider_support;
pub mod refine;
pub mod reporting;
pub mod review;
pub mod role;
pub mod sandbox;
pub mod scheduler;
pub mod security;
pub mod severity;
pub mod skillpack;
pub mod store;
pub mod toolsynth;
pub mod tools;
pub mod voice;
pub mod watcher;
pub mod webhook;
pub mod worker;
pub mod worktree;

pub use model::{
    apply, tasks_by_id, Acceptance, Agent, AgentStatus, Event, PrOutcomeKind, Reversibility, Role,
    RunState, RunStatus, Task, TaskOutcome, TaskStatus, Totals,
};
pub use store::{RunStore, StoreError};

/// Build the conventional run directory under a project root:
/// `<project>/.arccode/autonomous/<run-id>/`.
pub fn run_dir(project_root: &std::path::Path, run_id: &str) -> std::path::PathBuf {
    project_root
        .join(".arccode")
        .join("autonomous")
        .join(run_id)
}

/// Build the conventional worker worktree path:
/// `<project>/.arccode/worktrees/auto-<run-id>-<task-slug>/`.
pub fn worktree_dir(
    project_root: &std::path::Path,
    run_id: &str,
    task_slug: &str,
) -> std::path::PathBuf {
    project_root
        .join(".arccode")
        .join("worktrees")
        .join(format!("auto-{run_id}-{task_slug}"))
}

/// Build the conventional integration branch name: `arccode/auto/<run-id>`.
pub fn integration_branch(run_id: &str) -> String {
    format!("arccode/auto/{run_id}")
}
