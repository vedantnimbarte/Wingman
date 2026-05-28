//! Manager-only tools.
//!
//! These are the only tools registered on the manager's [`arccode_core::AgentLoop`].
//! Each is a thin Tool that translates JSON args from the model into a
//! [`crate::orchestrator::OrchestratorCommand`] and awaits the reply. Read-
//! only inspection tools (`list_dir`, `read_file`, `grep_tool`) come from
//! `arccode-tools::builtin` and are registered alongside these by
//! [`crate::manager::build_manager_registry`].

mod add_task;
mod assign_task;
mod finalize_task;
mod abort_task;
mod message_agent;
mod reassign_task;
mod run_acceptance;

pub use add_task::AddTask;
pub use assign_task::AssignTask;
pub use finalize_task::FinalizeTask;
pub use abort_task::AbortTask;
pub use message_agent::MessageAgent;
pub use reassign_task::ReassignTask;
pub use run_acceptance::RunAcceptance;
