//! Data model for the autonomous (pilot-mode) orchestrator.
//!
//! See `plan.md` § Data model. Two on-disk artefacts are kept per run:
//!
//! - `tasks.jsonl` — append-only event log; one [`Event`] per line.
//! - `state.json`  — atomic snapshot of the latest [`RunState`], rewritten
//!   after every event so the dashboard can read the current picture without
//!   replaying the log.
//!
//! Both live under `<project>/.arccode/autonomous/<run-id>/`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Stable identifier for an agent role. Roles are user-extensible via
/// markdown files at `~/.arccode/agents/<role>.md`; the variants here are the
/// roles the orchestrator schedules and reasons about directly.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Developer,
    Designer,
    Tester,
    Reviewer,
    Refactorer,
    MergeFixer,
    /// Escape hatch for skill packs (J12) that register their own roles.
    Custom(String),
}

impl Role {
    pub fn as_str(&self) -> &str {
        match self {
            Role::Developer => "developer",
            Role::Designer => "designer",
            Role::Tester => "tester",
            Role::Reviewer => "reviewer",
            Role::Refactorer => "refactorer",
            Role::MergeFixer => "merge-fixer",
            Role::Custom(s) => s.as_str(),
        }
    }
}

/// Lifecycle of a task in the DAG.
///
/// Transitions:
/// `pending` (deps unsatisfied) → `todo` (deps met, awaiting assignment) →
/// `in_progress` (worker spawned) → `review` (worker reported complete,
/// awaiting integration) → `done` (merged into integration branch),
/// or → `failed` | `blocked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Todo,
    InProgress,
    Review,
    Done,
    Failed,
    Blocked,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

/// Lifecycle of the run as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum RunStatus {
    #[default]
    Planning,
    AwaitingApproval,
    Running,
    Merging,
    Done,
    Failed,
    Aborted,
}

/// One executable acceptance check (E3). Workers must run every check and
/// attach results to `task_complete` before transitioning to `review`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Acceptance {
    /// Run a shell command; success = exit 0.
    Shell { cmd: String },
    /// Grep `pattern` in `path`; success = at least one match.
    Grep { pattern: String, path: String },
    /// HTTP GET; success = response JSON shape matches `must_match`.
    Http {
        url: String,
        #[serde(default)]
        must_match: serde_json::Value,
    },
}

/// Reversibility classification (R1). Orthogonal to `dangerous_paths`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum Reversibility {
    #[default]
    Trivial,
    Hard,
    Irreversible,
}

/// A task as planned and scheduled. Persistent fields only — agent assignment
/// lives in [`Agent`] and is keyed back here by `current_task`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub role: Role,
    pub title: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub deps: Vec<String>,
    /// Files / globs this task will write. Used by the write-set scheduler
    /// (E4) to avoid concurrent overlap.
    #[serde(default)]
    pub writes: Vec<String>,
    #[serde(default)]
    pub acceptance: Vec<Acceptance>,
    #[serde(default)]
    pub reversibility: Reversibility,
    #[serde(default)]
    pub reversibility_reason: Option<String>,
    pub status: TaskStatus,
    /// Agent currently working on the task, if any.
    #[serde(default)]
    pub agent: Option<String>,
    /// Worktree path relative to repo root.
    #[serde(default)]
    pub worktree: Option<String>,
    /// USD spent on this task so far (sum across attempts).
    #[serde(default)]
    pub usd: f64,
    /// Commits made by the worker on its task branch, oldest first.
    #[serde(default)]
    pub commits: Vec<String>,
    /// Outcome reported by the worker on completion (free-form summary).
    #[serde(default)]
    pub outcome: Option<TaskOutcome>,
}

impl Task {
    pub fn new(id: impl Into<String>, role: Role, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            role,
            title: title.into(),
            goal: String::new(),
            deps: Vec::new(),
            writes: Vec::new(),
            acceptance: Vec::new(),
            reversibility: Reversibility::default(),
            reversibility_reason: None,
            status: TaskStatus::Pending,
            agent: None,
            worktree: None,
            usd: 0.0,
            commits: Vec::new(),
            outcome: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutcome {
    pub summary: String,
    #[serde(default)]
    pub files_changed: Vec<String>,
}

/// A spawned worker process. Tracked separately from [`Task`] because a task
/// may be assigned, reassigned, or retried with a fresh worker (E5 ladder).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub role: Role,
    /// Task currently being executed by this worker, if any.
    #[serde(default)]
    pub current_task: Option<String>,
    /// OS process id of the spawned worker. None for the in-process manager.
    #[serde(default)]
    pub pid: Option<u32>,
    pub status: AgentStatus,
    /// Session id of the worker's own JSONL log under
    /// `<project>/.arccode/sessions/`. Lets `arccode session fork` operate on
    /// any worker's transcript.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    InProgress,
    Done,
    Failed,
    Aborted,
}

/// Aggregate token + USD counters across the run.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Totals {
    #[serde(default)]
    pub usd: f64,
    #[serde(default)]
    pub tokens_in: u64,
    #[serde(default)]
    pub tokens_out: u64,
}

/// Latest snapshot of the run. Written atomically after every event so a
/// reader can pick up the current state without replaying `tasks.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub run_id: String,
    pub goal: String,
    pub base_commit: String,
    pub integration_branch: String,
    #[serde(default)]
    pub status: RunStatus,
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub totals: Totals,
    /// URL of the PR opened by the orchestrator, once known.
    #[serde(default)]
    pub pr_url: Option<String>,
}

impl RunState {
    pub fn new(
        run_id: impl Into<String>,
        goal: impl Into<String>,
        base_commit: impl Into<String>,
        integration_branch: impl Into<String>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            goal: goal.into(),
            base_commit: base_commit.into(),
            integration_branch: integration_branch.into(),
            status: RunStatus::Planning,
            tasks: Vec::new(),
            agents: Vec::new(),
            totals: Totals::default(),
            pr_url: None,
        }
    }

    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub fn task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    pub fn agent(&self, id: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.id == id)
    }

    pub fn agent_mut(&mut self, id: &str) -> Option<&mut Agent> {
        self.agents.iter_mut().find(|a| a.id == id)
    }
}

/// One event in `tasks.jsonl`. State is reconstructed by replaying events.
///
/// All variants include an RFC-3339 `t` timestamp serialised as the `t` key,
/// so the on-wire shape matches the example in `plan.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Event {
    /// One-shot at run start. Captures goal + git base.
    #[serde(rename = "run.start")]
    RunStart {
        t: String,
        run_id: String,
        goal: String,
        base_commit: String,
        integration_branch: String,
    },

    /// Manager added a new task to the DAG.
    #[serde(rename = "task.create")]
    TaskCreate {
        t: String,
        id: String,
        role: Role,
        title: String,
        #[serde(default)]
        goal: String,
        #[serde(default)]
        deps: Vec<String>,
        #[serde(default)]
        writes: Vec<String>,
        #[serde(default)]
        acceptance: Vec<Acceptance>,
        #[serde(default)]
        reversibility: Reversibility,
        #[serde(default)]
        reversibility_reason: Option<String>,
    },

    /// Manager assigned a task to an agent. Implies the agent was spawned.
    #[serde(rename = "task.assign")]
    TaskAssign {
        t: String,
        id: String,
        agent: String,
        worktree: String,
    },

    /// Task transitioned status.
    #[serde(rename = "task.status")]
    TaskStatus {
        t: String,
        id: String,
        status: TaskStatus,
        #[serde(default)]
        outcome: Option<TaskOutcome>,
    },

    /// Worker invoked a tool. Recorded for the dashboard live log; full
    /// arguments live in the worker's own session JSONL.
    #[serde(rename = "task.tool")]
    TaskTool {
        t: String,
        id: String,
        agent: String,
        tool: String,
        #[serde(default)]
        input_hash: Option<String>,
        ok: bool,
    },

    /// Worker committed on its task branch.
    #[serde(rename = "task.commit")]
    TaskCommit { t: String, id: String, sha: String },

    /// Agent registration / bookkeeping.
    #[serde(rename = "agent.spawn")]
    AgentSpawn {
        t: String,
        agent: String,
        role: Role,
        #[serde(default)]
        pid: Option<u32>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Agent transitioned status (idle/in_progress/done/failed/aborted).
    #[serde(rename = "agent.status")]
    AgentStatus {
        t: String,
        agent: String,
        status: AgentStatus,
    },

    /// Token accounting delta for an agent. Aggregated into `totals` on replay.
    #[serde(rename = "agent.usd")]
    AgentUsd {
        t: String,
        agent: String,
        model: String,
        #[serde(default)]
        input_tokens: u64,
        #[serde(default)]
        output_tokens: u64,
        usd: f64,
    },

    /// Run-level status transition.
    #[serde(rename = "run.status")]
    RunStatusEv { t: String, status: RunStatus },

    /// Integration merge started.
    #[serde(rename = "run.merge.start")]
    RunMergeStart { t: String, branch: String },

    /// One task's branch was squash-merged into the integration branch.
    #[serde(rename = "run.merge.task")]
    RunMergeTask {
        t: String,
        id: String,
        #[serde(default = "default_strategy")]
        strategy: String,
        commit: String,
    },

    /// PR was opened (or push URL printed if `gh` is missing).
    #[serde(rename = "run.pr")]
    RunPr { t: String, url: String },

    /// Run terminated cleanly.
    #[serde(rename = "run.done")]
    RunDone { t: String },
}

fn default_strategy() -> String {
    "squash".into()
}

impl Event {
    pub fn timestamp(&self) -> &str {
        match self {
            Event::RunStart { t, .. }
            | Event::TaskCreate { t, .. }
            | Event::TaskAssign { t, .. }
            | Event::TaskStatus { t, .. }
            | Event::TaskTool { t, .. }
            | Event::TaskCommit { t, .. }
            | Event::AgentSpawn { t, .. }
            | Event::AgentStatus { t, .. }
            | Event::AgentUsd { t, .. }
            | Event::RunStatusEv { t, .. }
            | Event::RunMergeStart { t, .. }
            | Event::RunMergeTask { t, .. }
            | Event::RunPr { t, .. }
            | Event::RunDone { t } => t,
        }
    }
}

/// Apply one event to `state` in-place.
///
/// Used both by the live writer (after appending) and by `RunStore::load`
/// (when replaying from disk to reconstruct state). Unknown task / agent ids
/// are tolerated to keep replay robust against partially-written logs.
pub fn apply(state: &mut RunState, event: &Event) {
    match event {
        Event::RunStart {
            run_id,
            goal,
            base_commit,
            integration_branch,
            ..
        } => {
            state.run_id = run_id.clone();
            state.goal = goal.clone();
            state.base_commit = base_commit.clone();
            state.integration_branch = integration_branch.clone();
            state.status = RunStatus::Planning;
        }
        Event::TaskCreate {
            id,
            role,
            title,
            goal,
            deps,
            writes,
            acceptance,
            reversibility,
            reversibility_reason,
            ..
        } => {
            // Idempotent: replace if exists.
            let existing = state.tasks.iter().position(|t| &t.id == id);
            let task = Task {
                id: id.clone(),
                role: role.clone(),
                title: title.clone(),
                goal: goal.clone(),
                deps: deps.clone(),
                writes: writes.clone(),
                acceptance: acceptance.clone(),
                reversibility: *reversibility,
                reversibility_reason: reversibility_reason.clone(),
                status: TaskStatus::Pending,
                agent: None,
                worktree: None,
                usd: 0.0,
                commits: Vec::new(),
                outcome: None,
            };
            match existing {
                Some(i) => state.tasks[i] = task,
                None => state.tasks.push(task),
            }
        }
        Event::TaskAssign {
            id,
            agent,
            worktree,
            ..
        } => {
            let role = state.task(id).map(|t| t.role.clone());
            if let Some(t) = state.task_mut(id) {
                t.agent = Some(agent.clone());
                t.worktree = Some(worktree.clone());
                if t.status == TaskStatus::Pending {
                    t.status = TaskStatus::Todo;
                }
            }
            // Auto-register the agent on assignment if a later AgentSpawn
            // hasn't run yet — agents come and go through the lifecycle and
            // the manager isn't required to emit spawn-before-assign.
            if state.agent(agent).is_none() {
                if let Some(role) = role {
                    state.agents.push(Agent {
                        id: agent.clone(),
                        role,
                        current_task: Some(id.clone()),
                        pid: None,
                        status: AgentStatus::Idle,
                        session_id: None,
                    });
                }
            } else if let Some(a) = state.agent_mut(agent) {
                a.current_task = Some(id.clone());
            }
        }
        Event::TaskStatus {
            id,
            status,
            outcome,
            ..
        } => {
            if let Some(t) = state.task_mut(id) {
                t.status = *status;
                if let Some(o) = outcome {
                    t.outcome = Some(o.clone());
                }
            }
        }
        Event::TaskTool { .. } => {
            // Live-log only; no state mutation. Tracked in tasks.jsonl for the
            // dashboard.
        }
        Event::TaskCommit { id, sha, .. } => {
            if let Some(t) = state.task_mut(id) {
                t.commits.push(sha.clone());
            }
        }
        Event::AgentSpawn {
            agent,
            role,
            pid,
            session_id,
            ..
        } => {
            // Preserve `current_task` if the agent was auto-registered by an
            // earlier TaskAssign — spawn only refreshes pid / session_id.
            if let Some(existing) = state.agent_mut(agent) {
                existing.role = role.clone();
                if pid.is_some() {
                    existing.pid = *pid;
                }
                if session_id.is_some() {
                    existing.session_id = session_id.clone();
                }
            } else {
                state.agents.push(Agent {
                    id: agent.clone(),
                    role: role.clone(),
                    current_task: None,
                    pid: *pid,
                    status: AgentStatus::Idle,
                    session_id: session_id.clone(),
                });
            }
        }
        Event::AgentStatus { agent, status, .. } => {
            if let Some(a) = state.agent_mut(agent) {
                a.status = *status;
            }
        }
        Event::AgentUsd {
            agent,
            input_tokens,
            output_tokens,
            usd,
            ..
        } => {
            state.totals.usd += usd;
            state.totals.tokens_in += input_tokens;
            state.totals.tokens_out += output_tokens;
            if let Some(a) = state.agent(agent) {
                if let Some(task_id) = a.current_task.clone() {
                    if let Some(t) = state.task_mut(&task_id) {
                        t.usd += usd;
                    }
                }
            }
        }
        Event::RunStatusEv { status, .. } => {
            state.status = *status;
        }
        Event::RunMergeStart { .. } => {
            state.status = RunStatus::Merging;
        }
        Event::RunMergeTask { id, commit, .. } => {
            if let Some(t) = state.task_mut(id) {
                t.commits.push(commit.clone());
                t.status = TaskStatus::Done;
            }
        }
        Event::RunPr { url, .. } => {
            state.pr_url = Some(url.clone());
        }
        Event::RunDone { .. } => {
            state.status = RunStatus::Done;
        }
    }
}

/// Convenience: index tasks by id (for callers that prefer a map view).
pub fn tasks_by_id(state: &RunState) -> BTreeMap<&str, &Task> {
    state.tasks.iter().map(|t| (t.id.as_str(), t)).collect()
}
