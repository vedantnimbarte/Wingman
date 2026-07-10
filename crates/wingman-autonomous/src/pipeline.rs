//! End-to-end pilot pipeline: plan → workers → merge → PR.
//!
//! Glues together every other module so the CLI doesn't have to know
//! about the moving parts:
//!
//! 1. Spawn the orchestrator actor with [`orchestrator::spawn`].
//! 2. Build the manager [`wingman_core::AgentLoop`] via
//!    [`manager::build_manager`].
//! 3. Run [`manager::drive_to_completion`] until every task is terminal.
//! 4. If every task ended in `Done`, run
//!    [`worktree::merge_integration`] then
//!    [`pr::open_pull_request`].
//! 5. Cleanup worker worktrees.
//!
//! Used by both `wingman pilot run` (fresh run after planning) and
//! `wingman pilot resume` (existing run loaded from disk).

use std::path::PathBuf;
use std::sync::Arc;

use wingman_core::Provider;
use thiserror::Error;

use crate::manager::{build_manager, build_manager_registry, drive_to_completion, run_succeeded};
use crate::model::TaskStatus;
use crate::orchestrator::{self, OrchestratorConfig, WorkerSpawner};
use crate::pr::{self, CommandRunner, PrOutcome};
use crate::store::RunStore;
use crate::worktree::{self, IntegrationMergeOutcome};

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("manager: {0}")]
    Manager(#[from] crate::manager::ManagerError),
    #[error("orchestrator: {0}")]
    Orchestrator(#[from] crate::orchestrator::OrchestratorError),
    #[error("worktree: {0}")]
    Worktree(#[from] crate::worktree::WorktreeError),
    #[error("pr: {0}")]
    Pr(#[from] crate::pr::PrError),
    #[error("store: {0}")]
    Store(#[from] crate::store::StoreError),
    #[error("provider: {0}")]
    Provider(String),
}

/// Inputs the pipeline needs that aren't already encoded in the
/// [`RunStore`]'s [`crate::model::RunState`]. Kept as a separate struct
/// so the CLI and tests have an easy thing to build.
pub struct PipelineInputs {
    pub provider: Arc<dyn Provider>,
    pub manager_model: String,
    pub worker_spawner: WorkerSpawner,
    pub base_branch: String,
    /// Project root (`<repo>/`), used for worktree paths.
    pub project_root: PathBuf,
    /// Command runner for git/gh shellouts in pr.rs.
    pub command_runner: Box<dyn CommandRunner>,
    /// Whether to skip `gh pr create` entirely (the `--no-pr` flag).
    pub no_pr: bool,
    pub orchestrator_cfg: OrchestratorConfig,
    pub max_ticks: usize,
    /// Tier the run is operating at — recorded in the R3 escalation packet
    /// when the run blocks.
    pub tier: wingman_config::PilotTier,
    /// Worker model id — recorded in E6 stat records.
    pub worker_model: String,
    /// Where to append E6 cross-run stat records (`stats.jsonl`). `None`
    /// disables stats recording (tests, `--plan-only`).
    pub stats_path: Option<PathBuf>,
    /// Whether E1 auto-approved this plan — an input to the E8 auto-merge
    /// gate (auto-merge only fires for runs trusted from the start).
    pub auto_approved: bool,
    /// PR automation config (E8): auto-merge switch, CI requirement,
    /// severity gate.
    pub pr_config: wingman_config::PilotPrConfig,
    /// Security-pass config (R6): block severity, license allowlist.
    pub security_config: wingman_config::PilotSecurityConfig,
    /// E7 — run a per-task reviewer agent after the run. Off by default.
    pub run_reviewer: bool,
    /// J10 — run a critic agent before the auto-merge gate. Off by default.
    pub run_critic: bool,
    /// Model the reviewer/critic agents run on (usually `default_model`).
    pub reviewer_model: String,
    /// J11 default sandbox tier ("host" | "container" | "vm"); per-task
    /// tiers are escalated from this floor by `sandbox::select_tier`.
    pub sandbox_default_tier: String,
    /// J15 `[pilot.approval].dangerous_paths` globs. A write to one of these
    /// that the goal text never mentions raises a hard escalation trigger
    /// and blocks auto-merge. Empty disables the check.
    pub dangerous_paths: Vec<String>,
}

/// Outcome of one full pipeline run.
#[derive(Debug, Clone)]
pub struct PipelineOutcome {
    pub merged: Option<IntegrationMergeOutcome>,
    pub pr: Option<PrOutcome>,
    pub failed_tasks: Vec<String>,
    /// Path to the R3 escalation packet (`escalation.md`), written when the
    /// run blocked on failed/blocked tasks. `None` on a clean run.
    pub escalation_packet: Option<PathBuf>,
    /// E8 auto-merge decision, computed after the PR opens. `None` when no
    /// PR was opened (`--no-pr` or a blocked run).
    pub auto_merge: Option<crate::automerge::AutoMergeDecision>,
    /// E11 advisory checkpoint-hygiene violations, as `(task_id, reason)`.
    /// Empty on a clean run; advisory only (does not block).
    pub checkpoint_violations: Vec<(String, String)>,
    /// E7 per-task reviewer verdicts, as `(task_id, verdict)`. Empty when
    /// the reviewer pass is disabled.
    pub reviews: Vec<(String, crate::review::Verdict)>,
    /// J10 critic veto. `true` means the critic flagged a high+ risk and
    /// auto-merge was blocked regardless of the other gates.
    pub critic_vetoed: bool,
    /// J11 per-task sandbox tier chosen for the run, as `(task_id, tier)`.
    /// Selection is always computed; actual container/vm execution is a
    /// Docker/Firecracker-gated leaf invoked per tier.
    pub sandbox_tiers: Vec<(String, String)>,
    /// J15 hard escalation triggers detected over the integration diff +
    /// plan (dangerous-path-without-goal-mention, secrets, license-header
    /// edits). Empty on a clean run. Any blocking trigger vetoes auto-merge.
    pub escalation_triggers: Vec<crate::escalation::EscalationTrigger>,
}

/// Fallback rework gate when `[pilot.pr] reviewer_rework_severity` is unset or
/// unparseable. `High`: a task only reaches the reviewer after its acceptance
/// checks pass, so functional correctness is already established — the
/// reviewer's job is to catch genuinely severe issues, not to loop the run on
/// the medium-severity nitpicks an over-eager model emits on correct work.
const REVIEWER_REWORK_GATE: crate::severity::Severity = crate::severity::Severity::High;

/// Drive the run from its current state to completion.
///
/// Works for both fresh runs (RunStore freshly created, plan persisted)
/// and resumed runs (RunStore::load on existing dir). The orchestrator
/// + manager don't care which.
pub async fn run_to_completion(
    store: RunStore,
    inputs: PipelineInputs,
) -> Result<PipelineOutcome, PipelineError> {
    let state_at_start = store.state().clone();
    let integration_branch = state_at_start.integration_branch.clone();
    let project_root = inputs.project_root.clone();
    let run_id = state_at_start.run_id.clone();
    // Captured before the cfg is moved into the orchestrator — J15's runtime
    // cost trigger needs the cap at run end.
    let max_usd = inputs.orchestrator_cfg.max_usd;

    // Keep a handle to the provider for the post-run critic (J10) pass and
    // the E7 inline reviewer — `build_manager` consumes the original Arc.
    let aux_provider = inputs.provider.clone();

    // E7 — build the inline reviewer that gates each task's finalize. When
    // set, `run_reviewer` runs the reviewer at the Review→Done choke point
    // (race-free vs the manager) and sends rework verdicts back through the
    // retry ladder, instead of a batched post-run pass.
    // Captured before `inputs.manager_model` is moved into build_manager, so
    // the manager phase's tokens can be priced below.
    let manager_model = inputs.manager_model.clone();
    let reviewer: Option<orchestrator::Reviewer> = if inputs.run_reviewer {
        let provider = aux_provider.clone();
        let model = inputs.reviewer_model.clone();
        // The reviewer's rework bar is its own knob (`[pilot.pr]
        // reviewer_rework_severity`, default `high`), deliberately separate
        // from `auto_merge_max_severity` (which governs the merge decision):
        // the reviewer should loop only on real blockers, not the low nitpicks
        // a meticulous model tends to emit, or it deadlocks correct runs.
        let gate = inputs
            .pr_config
            .reviewer_rework_severity
            .parse::<crate::severity::Severity>()
            .unwrap_or(REVIEWER_REWORK_GATE);
        let repo = project_root.clone();
        let run_id_for_review = run_id.clone();
        let base_for_review = state_at_start.base_commit.clone();
        Some(std::sync::Arc::new(move |task: crate::model::Task| {
            let provider = provider.clone();
            let model = model.clone();
            let repo = repo.clone();
            let run_id = run_id_for_review.clone();
            let base = base_for_review.clone();
            Box::pin(async move {
                // Review the task's real diff. With no diff to show (git error,
                // empty change), approve rather than reject a change the model
                // can't see — the old sight-unseen path rejected correct work
                // and deadlocked the run.
                let diff = crate::worktree::task_diff(&repo, &run_id, &task.id, &base)?;
                review_task_inline(provider.as_ref(), &model, &task, &diff, gate).await
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        }))
    } else {
        None
    };

    let (handle, join) = orchestrator::spawn_full(
        store,
        inputs.orchestrator_cfg,
        inputs.worker_spawner,
        None,
        reviewer,
    );

    // Drive the manager loop. Manager system prompt is loaded inside
    // build_manager; the per-tick state block is injected by
    // drive_to_completion.
    let cwd = std::env::current_dir().unwrap_or_else(|_| project_root.clone());
    let registry = build_manager_registry(handle.clone(), cwd, project_root.clone());
    let mut agent = build_manager(inputs.provider, inputs.manager_model, registry, None);

    let manager_usage = drive_to_completion(&mut agent, &handle, inputs.max_ticks).await?;

    // Manager exited. Grab the final state and decide whether to merge.
    let final_state = handle.snapshot().await?;
    handle.shutdown().await;
    let _ = join.await;

    let run_dir = crate::run_dir(&project_root, &run_id);

    // Attribute the manager loop's tokens (previously dropped). Workers
    // already emit `agent.usd`; recording the manager here keeps run totals
    // honest and feeds the per-phase breakdown. Best-effort.
    if let Ok(mut s) = RunStore::load(&run_dir).await {
        record_phase_usage(&mut s, "manager", &manager_model, &manager_usage).await;
    }

    // E11 — advisory checkpoint-hygiene check over the recorded tool
    // stream. Surfaced (not blocked) so the operator can see when a
    // multi-file task skipped checkpointing.
    let checkpoint_violations = compute_checkpoint_violations(&run_dir, &final_state).await;

    // E6 — record one cross-run stat per task so the adaptive router and
    // J9 estimator have history to learn from on later runs.
    if let Some(stats_path) = &inputs.stats_path {
        record_run_stats(stats_path, &final_state, &inputs.worker_model);
    }

    // J11 — compute the sandbox tier each task should run in, escalating
    // from the configured default by its writes/acceptance/reversibility,
    // then degrading container/vm to host when no Docker daemon is
    // reachable so the run never wedges on a missing executor.
    let sandbox_tiers = compute_sandbox_tiers(
        &final_state,
        &inputs.sandbox_default_tier,
        inputs.command_runner.as_ref(),
    );

    let failed: Vec<String> = final_state
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Failed || t.status == TaskStatus::Blocked)
        .map(|t| t.id.clone())
        .collect();

    if !failed.is_empty() {
        tracing::warn!(
            target: "pilot::pipeline",
            failed = ?failed,
            "tasks ended in non-Done state; skipping merge + PR"
        );
        // R3 — write a handoff packet so the user has a single
        // openable artifact explaining where the run blocked and how to
        // resume, instead of just a log line. Surface any static J15
        // triggers (dangerous-path-without-goal-mention, secrets,
        // license-header edits) in the packet so the human page lands with
        // the actual escalation reasons, not just "a task failed". The diff
        // checks degrade gracefully when the integration branch was never
        // built (collect_diff_lines returns empty), so the dangerous-path
        // check over the run's recorded writes still fires.
        let escalation_triggers = detect_escalation_triggers(
            inputs.command_runner.as_ref(),
            &project_root,
            &final_state.base_commit,
            &integration_branch,
            &final_state,
            &inputs.dangerous_paths,
        );
        let packet = write_escalation_packet(
            &project_root,
            &run_id,
            &final_state,
            inputs.tier,
            &escalation_triggers,
        );
        return Ok(PipelineOutcome {
            merged: None,
            pr: None,
            failed_tasks: failed,
            escalation_packet: packet,
            auto_merge: None,
            checkpoint_violations,
            reviews: Vec::new(),
            critic_vetoed: false,
            sandbox_tiers,
            escalation_triggers,
        });
    }

    // Merge whenever any task completed — whether the manager finalized it
    // incrementally to Done or left it in Review for this end-of-run pass.
    // `merge_integration` runs the actual squash per task; `finalize_task`'s
    // incremental transition only records bookkeeping, it never runs git, so
    // Done tasks still need this merge or their work never reaches the
    // integration branch (and cleanup then deletes their branches).
    let need_merge = final_state
        .tasks
        .iter()
        .any(|t| matches!(t.status, TaskStatus::Review | TaskStatus::Done));

    let mut store = RunStore::load(&run_dir).await?;

    // E4 in-run conflict resolver: bridge the sync merge path to an async
    // agent that rewrites conflict markers to a clean resolution. Runs on the
    // multi-thread runtime via `block_in_place`. If it can't resolve, the
    // merge falls back to recording a merge-fixer task (the `Conflict` arm
    // below), so a bad resolution never lands — the file's markers are
    // re-checked before commit.
    let resolve_provider = aux_provider.clone();
    let resolve_model = inputs.reviewer_model.clone();
    let resolve_root = project_root.clone();
    let resolver = move |files: &[String]| -> bool {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(resolve_conflicts_inline(
                resolve_provider.as_ref(),
                &resolve_model,
                &resolve_root,
                files,
            ))
        })
    };

    let merge_outcome = if need_merge {
        match worktree::merge_integration_with_resolver(
            &project_root,
            &final_state.base_commit,
            &integration_branch,
            &final_state,
            Some(&resolver),
        ) {
            Ok(outcome) => {
                pr::finalize_all_review_tasks(&mut store, &final_state, &outcome.commits).await?;
                Some(outcome)
            }
            Err(crate::worktree::WorktreeError::Conflict { task_id, files }) => {
                // E4 auto-merge-fixer: a merge conflict no longer hard-errors
                // the run. Record a merge-fixer task capturing the conflicted
                // files so the conflict is structured, resumable work (a
                // `pilot resume` picks it up), then write the R3 escalation
                // packet and return a blocked outcome instead of a raw error.
                // ponytail: this queues the fix as a visible task; fully
                // autonomous live resolution (spawning a merge-fixer agent on
                // the conflicted checkout and retrying the merge in-process)
                // is the remaining provider-backed leaf — untestable headless.
                tracing::warn!(
                    target: "pilot::pipeline",
                    task = %task_id, files = ?files,
                    "merge conflict — recording a merge-fixer task and blocking the run"
                );
                record_merge_fixer_task(&mut store, &task_id, &files).await;
                let blocked_state = store.state().clone();
                let packet = write_escalation_packet(
                    &project_root,
                    &run_id,
                    &blocked_state,
                    inputs.tier,
                    &[],
                );
                return Ok(PipelineOutcome {
                    merged: None,
                    pr: None,
                    failed_tasks: vec![task_id],
                    escalation_packet: packet,
                    auto_merge: None,
                    checkpoint_violations,
                    reviews: Vec::new(),
                    critic_vetoed: false,
                    sandbox_tiers,
                    escalation_triggers: Vec::new(),
                });
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        // No task reached Review or Done — nothing to integrate. Still
        // create the integration branch ref (pointing at base) so downstream
        // status/PR steps have a branch to reference; merge_integration
        // against no mergeable tasks is a no-op beyond that.
        if !final_state.base_commit.is_empty() {
            let _ = worktree::merge_integration(
                &project_root,
                &final_state.base_commit,
                &integration_branch,
                &final_state,
            );
        }
        None
    };

    // Cleanup worker worktrees before opening the PR — keeps the repo
    // tidy if the PR step errors out. Also delete the per-task branches: by
    // here they've been squashed into the integration branch, so leaving them
    // only leaks refs that pile up across runs.
    let _removed = worktree::cleanup_worktrees(&project_root, &run_id);
    let _branches = worktree::cleanup_task_branches(&project_root, &run_id);

    if inputs.no_pr {
        store
            .append(crate::Event::RunDone { t: RunStore::now() })
            .await?;
        return Ok(PipelineOutcome {
            merged: merge_outcome,
            pr: None,
            failed_tasks: Vec::new(),
            escalation_packet: None,
            auto_merge: None,
            checkpoint_violations,
            reviews: Vec::new(),
            critic_vetoed: false,
            sandbox_tiers,
            escalation_triggers: Vec::new(),
        });
    }

    let snapshot_for_pr = store.state().clone();
    let pr_outcome = pr::open_pull_request(
        inputs.command_runner.as_ref(),
        &mut store,
        &project_root,
        &inputs.base_branch,
        &integration_branch,
        &snapshot_for_pr,
        None,
    )
    .await?;

    // R6 — security pass over the integration diff, feeding the E8 gate.
    let security_report = run_security_pass(
        inputs.command_runner.as_ref(),
        &project_root,
        &snapshot_for_pr.base_commit,
        &integration_branch,
        &inputs.security_config,
    );
    let sec_gate = inputs
        .security_config
        .block_severity
        .parse::<crate::severity::Severity>()
        .unwrap_or(crate::severity::Severity::Medium);
    let security_blocks = security_report.blocks_merge(sec_gate);
    if security_blocks {
        tracing::warn!(
            target: "pilot::pipeline",
            findings = security_report.findings.len(),
            "security pass blocks auto-merge"
        );
    }

    // J15 — hard escalation triggers over the plan + integration diff
    // (dangerous-path-without-goal-mention, secrets, license-header edits).
    // Any blocking trigger vetoes auto-merge regardless of the other gates.
    let mut escalation_triggers = detect_escalation_triggers(
        inputs.command_runner.as_ref(),
        &project_root,
        &snapshot_for_pr.base_commit,
        &integration_branch,
        &snapshot_for_pr,
        &inputs.dangerous_paths,
    );
    // J15 — fold the runtime escalation triggers (cost warn/halt, three
    // consecutive related-run failures, irreversible-task-ran) into the same
    // vec the E8 gate already vetoes on. `tests_before/after` stay `None`
    // until a cargo-test counter exists (that's the one signal with no live
    // producer); the other three fire from data already in scope.
    // ponytail: no test-count capture — NetNegativeTests stays dormant until
    // someone's willing to pay two full `cargo test` runs per pilot run.
    let recent_run_outcomes = recent_run_outcomes(&project_root, &run_id);
    escalation_triggers.extend(crate::escalation::check_runtime(
        &crate::escalation::RuntimeSignals {
            state: &snapshot_for_pr,
            task: snapshot_for_pr
                .tasks
                .iter()
                .find(|t| matches!(t.reversibility, crate::model::Reversibility::Irreversible)),
            tests_before: None,
            tests_after: None,
            max_usd,
            recent_run_outcomes: &recent_run_outcomes,
        },
    ));
    let dangerous_paths_touched = escalation_triggers.iter().any(|t| {
        matches!(
            t,
            crate::escalation::EscalationTrigger::DangerousPathTouched { .. }
        )
    });
    let escalation_blocks = escalation_triggers.iter().any(|t| t.blocks_auto_merge());
    if !escalation_triggers.is_empty() {
        tracing::warn!(
            target: "pilot::pipeline",
            triggers = ?escalation_triggers.iter().map(|t| t.short_label()).collect::<Vec<_>>(),
            blocks_merge = escalation_blocks,
            "J15 escalation triggers fired"
        );
    }

    // E7 — the per-task reviewer now runs INLINE at each task's finalize
    // choke point (see the `reviewer` closure + `spawn_full` above), so a
    // rework verdict bounces the task back through the retry ladder during
    // the run instead of only annotating the gate afterward. By the time a
    // task is Done here it has already passed inline review, so there's no
    // separate post-run pass and the gate's review severity is None.
    // ponytail: a task that was batch-finalized by the pipeline (rather than
    // the manager calling finalize_task) skips the inline gate — that path is
    // only hit when the manager never finalized incrementally, which the
    // common flow avoids.
    let reviews: Vec<(String, crate::review::Verdict)> = Vec::new();
    let review_max_severity: Option<crate::severity::Severity> = None;

    // J10 — critic pass (opt-in). A high+ risk vetoes auto-merge.
    let mut critic_usage = wingman_core::Usage::default();
    let critic_vetoed = if inputs.run_critic {
        run_critic_pass(
            aux_provider.as_ref(),
            &inputs.reviewer_model,
            &snapshot_for_pr,
            &mut critic_usage,
        )
        .await
    } else {
        false
    };
    record_phase_usage(&mut store, "critic", &inputs.reviewer_model, &critic_usage).await;

    // E8 — auto-merge gate. Combine the available signals and decide
    // whether to merge automatically. When it decides Merge and the PR was
    // opened by gh, we issue `gh pr merge`.
    let auto_merge_decision = decide_and_maybe_merge(
        inputs.command_runner.as_ref(),
        &project_root,
        &inputs.pr_config,
        inputs.auto_approved,
        inputs.tier,
        // J15 blocking triggers veto the merge alongside the R6 security gate.
        security_blocks || escalation_blocks,
        inputs.run_reviewer,
        review_max_severity,
        critic_vetoed,
        dangerous_paths_touched,
        &pr_outcome,
    );

    // J8 — regenerate the durable project knowledge layer now that the run
    // merged: an architecture map from the crates' `pub mod`s + one decision
    // record. Best-effort; a knowledge write must never fail the run.
    // ponytail: a plain function call, not a "knowledge-keeper agent" —
    // render_architecture is deterministic; an LLM to invoke it is ceremony.
    regenerate_knowledge(&project_root, &snapshot_for_pr);

    Ok(PipelineOutcome {
        merged: merge_outcome,
        pr: Some(pr_outcome),
        failed_tasks: Vec::new(),
        escalation_packet: None,
        auto_merge: Some(auto_merge_decision),
        checkpoint_violations,
        reviews,
        critic_vetoed,
        sandbox_tiers,
        escalation_triggers,
    })
}

/// J11 — compute the per-task sandbox tier for the run from the configured
/// default floor + each task's writes/acceptance/reversibility. Pure;
/// actual container/vm execution (`sandbox::run_in_container`) is the
/// Docker/Firecracker-gated leaf invoked per chosen tier.
fn compute_sandbox_tiers(
    state: &crate::model::RunState,
    default_tier: &str,
    runner: &dyn CommandRunner,
) -> Vec<(String, String)> {
    let floor = crate::sandbox::SandboxTier::parse(default_tier);
    // Probe Docker once; container/vm tiers degrade to host when absent.
    let docker = crate::sandbox::docker_available(runner);
    state
        .tasks
        .iter()
        .map(|t| {
            let requested = crate::sandbox::select_tier(t, floor);
            let effective = if docker {
                requested
            } else {
                crate::sandbox::resolve_effective_tier(requested, runner).0
            };
            (t.id.clone(), effective.as_str().to_string())
        })
        .collect()
}

/// E8 — evaluate the auto-merge gate and, when it says Merge and `gh`
/// opened the PR, run `gh pr merge --squash --auto`. Returns the decision.
#[allow(clippy::too_many_arguments)]
fn decide_and_maybe_merge(
    runner: &dyn CommandRunner,
    project_root: &std::path::Path,
    pr_config: &wingman_config::PilotPrConfig,
    auto_approved: bool,
    tier: wingman_config::PilotTier,
    security_blocks: bool,
    reviewed: bool,
    review_max_severity: Option<crate::severity::Severity>,
    critic_vetoes: bool,
    dangerous_paths_touched: bool,
    pr_outcome: &PrOutcome,
) -> crate::automerge::AutoMergeDecision {
    use crate::severity::Severity;
    let gate = pr_config
        .auto_merge_max_severity
        .parse::<Severity>()
        .unwrap_or(Severity::Low);
    // Autopilot may auto-merge from a notify-only window too; copilot only
    // from a clean auto-approve. We model "trusted from the start" as
    // auto_approved, relaxed for autopilot.
    let tier_was_auto = auto_approved || tier == wingman_config::PilotTier::Autopilot;
    // CI status only matters when the gate requires it and `gh` actually
    // opened the PR (otherwise there's nothing to query). A pending/unknown
    // result maps to `None`, which `decide_auto_merge` treats as "hold".
    let ci_green = if pr_config.require_ci_green && pr_outcome.created_by_gh {
        query_ci_status(runner, project_root, &pr_outcome.url)
    } else {
        None
    };
    let decision = crate::automerge::decide_auto_merge(&crate::automerge::AutoMergeInputs {
        config_auto_merge: pr_config.auto_merge,
        tier_was_auto,
        ci_green,
        require_ci_green: pr_config.require_ci_green,
        reviewed,                // whether a per-task review actually ran
        review_max_severity,     // E7 per-task reviewer (wired below)
        security_blocks,         // R6 security pass + J15 blocking triggers
        critic_vetoes,           // J10 critic (wired below)
        dangerous_paths_touched, // J15 dangerous-path-without-goal-mention
        merge_max_severity: gate,
    });
    if decision.is_merge() && pr_outcome.created_by_gh {
        let out = runner.run(
            "gh",
            &["pr", "merge", "--squash", "--auto", &pr_outcome.url],
            project_root,
        );
        match out {
            Ok(o) if o.success() => {
                tracing::info!(target: "pilot::pipeline", url = %pr_outcome.url, "auto-merged PR");
            }
            Ok(o) => {
                tracing::warn!(target: "pilot::pipeline", stderr = %o.stderr, "gh pr merge failed")
            }
            Err(e) => tracing::warn!(target: "pilot::pipeline", error = %e, "gh pr merge errored"),
        }
    }
    decision
}

/// E8 — query CI status for an opened PR via `gh pr checks <url> --json
/// state`. Returns `Some(true)` when every check passed (treating
/// `SKIPPED`/`NEUTRAL` as passing), `Some(false)` when any check failed,
/// and `None` when checks are still pending, none are configured, or `gh`
/// is unavailable. `gh pr checks` exits non-zero while checks are
/// failing/pending but still prints the JSON, so we parse stdout regardless
/// of the exit status.
fn query_ci_status(
    runner: &dyn CommandRunner,
    project_root: &std::path::Path,
    pr_url: &str,
) -> Option<bool> {
    let out = runner
        .run(
            "gh",
            &["pr", "checks", pr_url, "--json", "state"],
            project_root,
        )
        .ok()?;
    let parsed: serde_json::Value = serde_json::from_str(out.stdout.trim()).ok()?;
    let arr = parsed.as_array()?;
    if arr.is_empty() {
        return None; // no checks configured → nothing to gate on
    }
    let mut any_pending = false;
    for c in arr {
        match c.get("state").and_then(|s| s.as_str()) {
            Some("SUCCESS") | Some("SKIPPED") | Some("NEUTRAL") => {}
            Some("PENDING") | Some("QUEUED") | Some("IN_PROGRESS") | Some("REQUESTED")
            | Some("WAITING") | Some("EXPECTED") => any_pending = true,
            // FAILURE, ERROR, CANCELLED, TIMED_OUT, ACTION_REQUIRED, STALE…
            Some(_) => return Some(false),
            None => any_pending = true,
        }
    }
    if any_pending {
        None
    } else {
        Some(true)
    }
}

/// R6 — run the security pass over the integration diff. Currently the
/// dependency-free built-in scan (secrets via prefix + entropy) over the
/// added lines of `git diff <base>..<integration>`. External scanners
/// (`gitleaks`, `cargo audit`) and license scanning layer on once their
/// inputs are gathered; this is the always-on baseline.
fn run_security_pass(
    runner: &dyn CommandRunner,
    project_root: &std::path::Path,
    base_commit: &str,
    integration_branch: &str,
    _cfg: &wingman_config::PilotSecurityConfig,
) -> crate::security::SecurityReport {
    let mut report = crate::security::SecurityReport::default();
    let diff = collect_diff_lines(runner, project_root, base_commit, integration_branch);
    report.extend(crate::security::scan_secrets(&diff.added));
    report
}

/// Parsed lines from `git diff <base>..<branch>`, used by both the R6
/// security pass and the J15 escalation checks. `added` is the `+` lines
/// (sans `+`); `changed` is every `+` *and* `-` line — license-header
/// detection cares about removals too. Each line is paired with its file.
#[derive(Debug, Default)]
struct DiffLines {
    added: Vec<(String, String)>,
    changed: Vec<(String, String)>,
}

/// Run `git diff --unified=0 <base>..<branch>` and split it into added /
/// changed lines. Returns empty on an empty base commit or a failed diff.
fn collect_diff_lines(
    runner: &dyn CommandRunner,
    project_root: &std::path::Path,
    base_commit: &str,
    integration_branch: &str,
) -> DiffLines {
    let mut out = DiffLines::default();
    if base_commit.is_empty() {
        return out;
    }
    let range = format!("{base_commit}..{integration_branch}");
    let diff = match runner.run("git", &["diff", "--unified=0", &range], project_root) {
        Ok(o) if o.success() => o.stdout,
        _ => return out,
    };
    // Track both the a-side and b-side file so removals in a deleted file
    // (`+++ /dev/null`) still attribute to the original path.
    let mut file_a = String::new();
    let mut file_b = String::new();
    for line in diff.lines() {
        if let Some(p) = line.strip_prefix("--- a/") {
            file_a = p.to_string();
        } else if let Some(p) = line.strip_prefix("+++ b/") {
            file_b = p.to_string();
        } else if let Some(p) = line.strip_prefix("--- ") {
            file_a = p.to_string(); // e.g. "/dev/null"
        } else if let Some(p) = line.strip_prefix("+++ ") {
            file_b = p.to_string();
        } else if let Some(rest) = line.strip_prefix('+') {
            let f = if file_b == "/dev/null" {
                &file_a
            } else {
                &file_b
            };
            out.added.push((f.clone(), rest.to_string()));
            out.changed.push((f.clone(), rest.to_string()));
        } else if let Some(rest) = line.strip_prefix('-') {
            let f = if file_b == "/dev/null" {
                &file_a
            } else {
                &file_b
            };
            out.changed.push((f.clone(), rest.to_string()));
        }
    }
    out
}

/// J15 — detect the static (plan + diff) hard-escalation triggers over the
/// integration diff and this run's recorded writes. `goal` gates the
/// dangerous-path check (a touched dangerous path the goal never mentioned
/// escalates; one it asked for doesn't). Pure aside from the `git diff`
/// shellout in [`collect_diff_lines`].
fn detect_escalation_triggers(
    runner: &dyn CommandRunner,
    project_root: &std::path::Path,
    base_commit: &str,
    integration_branch: &str,
    state: &crate::model::RunState,
    dangerous_paths: &[String],
) -> Vec<crate::escalation::EscalationTrigger> {
    let mut triggers = Vec::new();
    // Dangerous-path-without-goal-mention, from the run's recorded writes.
    if !dangerous_paths.is_empty() {
        let writes: Vec<String> = state
            .tasks
            .iter()
            .flat_map(|t| t.writes.iter().cloned())
            .collect();
        let hits = crate::approval::paths_matching(&writes, dangerous_paths);
        triggers.extend(crate::escalation::dangerous_path_triggers(
            &hits,
            &state.goal,
        ));
    }
    // Secrets + license-header edits, from the diff.
    let diff = collect_diff_lines(runner, project_root, base_commit, integration_branch);
    triggers.extend(crate::escalation::secret_triggers(&diff.added));
    triggers.extend(crate::escalation::license_header_triggers(&diff.changed));
    triggers
}

/// Record a phase's token usage as a `phase:<name>`-tagged `agent.usd`
/// event so it rolls into `state.totals` and the per-phase breakdown. The
/// synthetic agent id is never a registered agent, so `apply` only updates
/// totals — no spurious per-task attribution. Best-effort: a failed append
/// is logged and swallowed so instrumentation never breaks a run. (Cache
/// read/write tokens aren't in the event schema yet, so only fresh
/// input/output are recorded.)
async fn record_phase_usage(
    store: &mut RunStore,
    phase: &str,
    model: &str,
    usage: &wingman_core::Usage,
) {
    if usage.input_tokens == 0 && usage.output_tokens == 0 {
        return;
    }
    let usd = wingman_core::pricing::price_for(model)
        .map(|p| p.cost(usage))
        .unwrap_or(0.0);
    if let Err(e) = store
        .append(crate::Event::AgentUsd {
            t: RunStore::now(),
            agent: format!("phase:{phase}"),
            model: model.to_string(),
            input_tokens: usage.input_tokens as u64,
            output_tokens: usage.output_tokens as u64,
            usd,
        })
        .await
    {
        tracing::warn!(target: "pilot::pipeline", phase, error = %e, "failed to record phase token usage");
    }
}

/// One-shot text completion: send a system+user prompt and concatenate
/// the assistant's `TextDelta`s. Used by the E7 reviewer and J10 critic
/// passes, which expect a single JSON object back.
async fn complete_text(
    provider: &dyn Provider,
    model: &str,
    system: &str,
    user: &str,
) -> Result<(String, wingman_core::Usage), PipelineError> {
    use wingman_core::{
        CacheBreakpoint, CompletionRequest, ContentBlock, Message, Role as ApiRole, StreamEvent,
        Usage,
    };
    use futures::StreamExt;
    let req = CompletionRequest {
        model: model.to_string(),
        system: Some(system.to_string()),
        messages: vec![Message {
            role: ApiRole::User,
            content: vec![ContentBlock::Text {
                text: user.to_string(),
            }],
        }],
        tools: vec![],
        max_tokens: 2048,
        temperature: None,
        // The reviewer pass reuses this system prompt once per task; caching
        // it lets calls 2..N read it back instead of re-sending it each time.
        cache_breakpoints: vec![CacheBreakpoint::AfterSystem],
    };
    let mut stream = provider
        .complete(req)
        .await
        .map_err(|e| PipelineError::Provider(e.to_string()))?;
    let mut out = String::new();
    let mut usage = Usage::default();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(StreamEvent::TextDelta { text }) => out.push_str(&text),
            Ok(StreamEvent::Usage { usage: u }) => usage.add(&u),
            _ => {}
        }
    }
    Ok((out, usage))
}

/// Extract the first top-level JSON object from a possibly-chatty reply
/// (models sometimes wrap JSON in prose or fences).
fn extract_json(s: &str) -> &str {
    match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if b > a => &s[a..=b],
        _ => s,
    }
}

/// E7 — run a reviewer agent per task and collect verdicts. Parse
/// failures default to Approve (a broken reviewer must not wedge the run);
/// the orchestrator still has the security + critic gates.
/// E7 — review one task inline at its finalize choke point. Returns
/// `Some(rework_notes)` when the task should go back for rework (verdict
/// Rework, or a finding at/above `block_gate`), or `None` to approve. A
/// call/parse failure defaults to approve (fail-open, same as the post-run
/// pass) so a flaky reviewer can't wedge the run.
/// Strip a single leading/trailing markdown code fence if the model wrapped
/// its answer in one, so we write the raw file content, not ```-decorated text.
fn strip_fence(s: &str) -> String {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        // drop the opening fence line (may carry a language tag) and a
        // closing ``` if present.
        let body = rest.split_once('\n').map(|x| x.1).unwrap_or("");
        let body = body.strip_suffix("```").unwrap_or(body);
        return body.trim_end().to_string();
    }
    t.to_string()
}

/// E4 in-run merge-conflict resolver. For each conflicted file (which holds
/// git conflict markers), ask the model for the fully resolved contents and
/// write them back. Returns `true` only if every file was rewritten with no
/// markers left; any error or a still-conflicted result returns `false`, so
/// the caller falls back to the merge-fixer task instead of committing garbage.
async fn resolve_conflicts_inline(
    provider: &dyn Provider,
    model: &str,
    repo_root: &std::path::Path,
    files: &[String],
) -> bool {
    const SYSTEM: &str = "You resolve a git merge conflict in a single file. You are given \
        the file's full contents including conflict markers (<<<<<<<, =======, >>>>>>>). \
        Reply with ONLY the fully resolved file contents — every marker removed, both \
        sides' intent integrated. No commentary, no markdown fences.";
    for f in files {
        let path = repo_root.join(f);
        let Ok(content) = std::fs::read_to_string(&path) else {
            return false;
        };
        if !content.contains("<<<<<<<") {
            continue; // nothing to resolve in this file
        }
        let user = format!("Path: {f}\n\n{content}");
        let Ok((answer, _usage)) = complete_text(provider, model, SYSTEM, &user).await else {
            tracing::warn!(target: "pilot::pipeline", file = %f, "conflict resolver: model call failed");
            return false;
        };
        let resolved = strip_fence(&answer);
        if resolved.trim().is_empty()
            || resolved.contains("<<<<<<<")
            || resolved.contains(">>>>>>>")
        {
            return false;
        }
        // Preserve a trailing newline like most source files carry.
        let resolved = if resolved.ends_with('\n') {
            resolved
        } else {
            format!("{resolved}\n")
        };
        if std::fs::write(&path, resolved.as_bytes()).is_err() {
            return false;
        }
    }
    true
}

async fn review_task_inline(
    provider: &dyn Provider,
    model: &str,
    task: &crate::model::Task,
    diff: &str,
    block_gate: crate::severity::Severity,
) -> Option<String> {
    const SYSTEM: &str = "You are a pragmatic code reviewer. Review the task's actual diff \
        (below) against its stated goal and reply with ONLY a JSON object: \
        {\"verdict\":\"approve\"|\"rework\", \
        \"summary\":\"...\", \"findings\":[{\"severity\":\"low|medium|high|critical\", \
        \"message\":\"...\"}]}. Judge only the diff shown; do not demand changes \
        for code you cannot see. Approve when the diff satisfies the goal. Only \
        request rework for a concrete defect — a bug, a broken build, or the goal \
        left unmet — and record it as a medium-or-higher finding. Do not rework \
        over style nits or preferences; file those as low-severity and approve.";
    let summary = task
        .outcome
        .as_ref()
        .map(|o| o.summary.as_str())
        .unwrap_or("(no summary)");
    // Bound the diff so a large change can't blow the reviewer's context.
    const MAX_DIFF_CHARS: usize = 12_000;
    let diff_block: String = if diff.chars().count() > MAX_DIFF_CHARS {
        let head: String = diff.chars().take(MAX_DIFF_CHARS).collect();
        format!("{head}\n… (diff truncated)")
    } else {
        diff.to_string()
    };
    let user = format!(
        "Task #{}: {}\nRole: {}\nGoal: {}\nWorker summary: {}\n\n## Diff\n```diff\n{}\n```",
        task.id,
        task.title,
        task.role.as_str(),
        task.goal,
        summary,
        diff_block,
    );
    let (text, _usage) = complete_text(provider, model, SYSTEM, &user).await.ok()?;
    let report = crate::review::parse_review(extract_json(&text)).ok()?;
    if report.next_status(block_gate) == TaskStatus::Todo {
        Some(report.rework_notes(block_gate))
    } else {
        None
    }
}

/// J10 — run a critic on the whole run; returns true if it vetoes
/// auto-merge (any high+ risk). Parse/call failures default to no veto.
async fn run_critic_pass(
    provider: &dyn Provider,
    model: &str,
    state: &crate::model::RunState,
    usage: &mut wingman_core::Usage,
) -> bool {
    const SYSTEM: &str = "You are an adversarial critic on a different model family than the \
        author. Find what could break this work. Reply with ONLY a JSON object: \
        {\"summary\":\"...\",\"risks\":[{\"severity\":\"low|medium|high|critical\", \
        \"description\":\"...\"}]}.";
    let tasks: Vec<String> = state
        .tasks
        .iter()
        .map(|t| format!("- #{} [{}] {}", t.id, t.role.as_str(), t.title))
        .collect();
    let user = format!("Goal: {}\nTasks:\n{}", state.goal, tasks.join("\n"));
    match complete_text(provider, model, SYSTEM, &user).await {
        Ok((text, u)) => {
            usage.add(&u);
            match crate::critic::parse_critic(extract_json(&text)) {
                Ok(report) => report.vetoes_auto_merge(),
                Err(_) => false,
            }
        }
        Err(e) => {
            tracing::warn!(target: "pilot::pipeline", error = %e, "critic call failed");
            false
        }
    }
}

/// E11 — read the event log and flag tasks that reached a terminal state
/// without satisfying checkpoint hygiene. Advisory; best-effort (a read
/// error yields no violations rather than failing the run).
async fn compute_checkpoint_violations(
    run_dir: &std::path::Path,
    state: &crate::model::RunState,
) -> Vec<(String, String)> {
    let events = match RunStore::load(run_dir).await {
        Ok(store) => store.read_events().await.unwrap_or_default(),
        Err(_) => return Vec::new(),
    };
    let mut violations = Vec::new();
    for task in &state.tasks {
        if !matches!(task.status, TaskStatus::Review | TaskStatus::Done) {
            continue;
        }
        let calls = crate::checkpoint::tool_calls_for_task(&events, &task.id);
        if let crate::checkpoint::CheckpointVerdict::Violation { reason } =
            crate::checkpoint::verify(&calls)
        {
            violations.push((task.id.clone(), reason));
        }
    }
    violations
}

/// E6 — append one [`crate::learning::StatRecord`] per task to the stats
/// log so later runs can route adaptively and estimate from history.
fn record_run_stats(
    stats_path: &std::path::Path,
    state: &crate::model::RunState,
    worker_model: &str,
) {
    for task in &state.tasks {
        let rec = crate::learning::StatRecord {
            run_id: state.run_id.clone(),
            role: task.role.as_str().to_string(),
            model: worker_model.to_string(),
            task_kind: None,
            // Proxy: a task that reached Done passed; refinement (true
            // first-try detection) lands when the retry ladder records
            // attempt counts.
            first_try_ok: task.status == TaskStatus::Done,
            pr_outcome: None, // R2 poller backfills this later
            goal: state.goal.clone(),
            t: RunStore::now(),
        };
        if let Err(e) = crate::learning::append_stat(stats_path, &rec) {
            tracing::warn!(target: "pilot::pipeline", error = %e, "failed to append stat record");
        }
    }
}

/// E4 — record a merge-fixer task on a merge conflict. Appends a
/// `task.create` for a [`crate::model::Role::MergeFixer`] whose `writes` are
/// the conflicted files and whose goal points at the conflicting task, so the
/// conflict is durable, resumable work rather than a lost hard error.
/// Best-effort: a failed append is logged, not surfaced (the run is already
/// blocking on the conflict).
async fn record_merge_fixer_task(
    store: &mut RunStore,
    conflicting_task_id: &str,
    files: &[String],
) {
    let id = format!("merge-fixer-{conflicting_task_id}");
    let ev = crate::Event::TaskCreate {
        t: RunStore::now(),
        id,
        role: crate::model::Role::MergeFixer,
        title: format!("Resolve merge conflict from task {conflicting_task_id}"),
        goal: format!(
            "Task {conflicting_task_id} conflicts with earlier integration work in: {}. \
             Resolve the conflict preserving both sides' intent, re-run acceptance, and commit.",
            files.join(", ")
        ),
        deps: Vec::new(),
        writes: files.to_vec(),
        acceptance: Vec::new(),
        reversibility: Default::default(),
        reversibility_reason: None,
    };
    if let Err(e) = store.append(ev).await {
        tracing::warn!(target: "pilot::pipeline", error = %e, "failed to record merge-fixer task");
    }
}

/// J15 — the last-3 (run_id, ok) outcomes from run history, newest last,
/// excluding the current run (still Running). Feeds
/// [`crate::escalation::check_runtime`]'s RepeatedFailures trigger. Reads
/// what `dashboard::load_all_run_states` already persists — no new store.
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
        .map(|s| (s.run_id, s.status == crate::model::RunStatus::Done))
        .collect()
}

/// J8 — regenerate the durable knowledge layer under `.wingman/knowledge/`
/// after a run merges: an `architecture.md` module map + one appended
/// decision record. Best-effort; every failure is logged and swallowed so a
/// knowledge write can never fail the run.
fn regenerate_knowledge(project_root: &std::path::Path, state: &crate::model::RunState) {
    let dir = crate::knowledge::knowledge_dir(project_root);

    // Module map: each `crates/<name>/src/lib.rs` → its `pub mod`s.
    let crates = discover_crate_modules(&project_root.join("crates"));
    let arch = crate::knowledge::render_architecture(&crates);
    if let Err(e) = std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::write(dir.join("architecture.md"), arch))
    {
        tracing::warn!(target: "pilot::pipeline", error = %e, "failed to write architecture.md");
    }

    // One decision record for the run: the goal is what was decided, the
    // top task summaries the rationale.
    let rationale = state
        .tasks
        .iter()
        .filter_map(|t| t.outcome.as_ref().map(|o| o.summary.clone()))
        .collect::<Vec<_>>()
        .join("; ");
    let rec = crate::knowledge::DecisionRecord {
        run_id: state.run_id.clone(),
        t: RunStore::now(),
        decision: state.goal.clone(),
        rationale,
    };
    if let Err(e) = crate::knowledge::append_decision(&crate::knowledge::decisions_path(&dir), &rec)
    {
        tracing::warn!(target: "pilot::pipeline", error = %e, "failed to append decision record");
    }
    // ponytail: hotspots are computed by knowledge::hotspots_from_observations
    // but nothing reads a persisted hotspots file yet (the E4 scheduler takes
    // them in-memory), so persisting one here would be write-only. Add it when
    // the scheduler learns to load cross-run hotspots.
}

/// Walk `crates/*/src/lib.rs` and extract each crate's `pub mod <name>;`
/// declarations. Pure-ish (reads the filesystem); returns
/// `(crate_name, sorted module names)` for `render_architecture`.
fn discover_crate_modules(crates_dir: &std::path::Path) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(crates_dir) else {
        return out;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        let lib = e.path().join("src").join("lib.rs");
        let Ok(src) = std::fs::read_to_string(&lib) else {
            continue;
        };
        let mut mods: Vec<String> = src
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                l.strip_prefix("pub mod ")
                    .map(|rest| rest.trim_end_matches(';').split_whitespace().next().unwrap_or(""))
                    .filter(|m| !m.is_empty())
                    .map(|m| m.to_string())
            })
            .collect();
        mods.sort();
        mods.dedup();
        out.push((name, mods));
    }
    out
}

/// R3 — render + write the escalation packet for a blocked run. Returns
/// the packet path on success, or `None` if writing failed (best-effort;
/// a failed packet write must not mask the run's real failure).
fn write_escalation_packet(
    project_root: &std::path::Path,
    run_id: &str,
    state: &crate::model::RunState,
    tier: wingman_config::PilotTier,
    triggers: &[crate::escalation::EscalationTrigger],
) -> Option<PathBuf> {
    let blocked_task = state
        .tasks
        .iter()
        .find(|t| matches!(t.status, TaskStatus::Failed | TaskStatus::Blocked));
    let packet = crate::handoff::HandoffPacket {
        state,
        tier,
        blocked_task,
        triggers,
        attempts: &[],
        why_stuck: None,
        suggested_next: None,
    };
    let run_dir = crate::run_dir(project_root, run_id);
    match crate::handoff::write_packet(&run_dir, &packet) {
        Ok(path) => Some(path),
        Err(e) => {
            tracing::warn!(target: "pilot::pipeline", error = %e, "failed to write escalation packet");
            None
        }
    }
}

/// Mark tasks stuck in `InProgress` as `Failed` so the retry watchdog
/// picks them up on resume. Used at the start of `wingman pilot resume`.
pub async fn mark_stale_in_progress_failed(
    store: &mut RunStore,
) -> Result<Vec<String>, PipelineError> {
    let stuck: Vec<String> = store
        .state()
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::InProgress)
        .map(|t| t.id.clone())
        .collect();
    for id in &stuck {
        store
            .append(crate::Event::TaskStatus {
                t: RunStore::now(),
                id: id.clone(),
                status: TaskStatus::Failed,
                outcome: None,
            })
            .await?;
    }
    Ok(stuck)
}

/// Convenience: did the pipeline succeed end-to-end?
pub fn pipeline_succeeded(state: &crate::model::RunState) -> bool {
    run_succeeded(state) && state.pr_url.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Event, Role, RunStatus, Task, TaskStatus};
    use crate::orchestrator::{fake_happy_spawner, OrchestratorConfig};
    use crate::pr::{CommandOut, CommandRunner};
    use wingman_core::{
        AgentEvent, AgentStop, CompletionRequest, ContentBlock, Message, Provider,
        ProviderCapabilities, ProviderEventStream, Role as ApiRole, StopReason, StreamEvent, Usage,
    };
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// Scripted provider: each `complete()` call peeks at the LAST user
    /// message in the request to figure out where the run is and returns
    /// the appropriate tool-use block. The manager system prompt + the
    /// rendered state block are part of the input so the provider can
    /// branch on what the manager is seeing.
    struct ScriptedProvider {
        call_count: Mutex<u32>,
    }

    impl ScriptedProvider {
        fn new() -> Self {
            Self {
                call_count: Mutex::new(0),
            }
        }
    }

    fn tool_use(name: &str, args: serde_json::Value) -> ContentBlock {
        ContentBlock::ToolUse {
            id: format!("call-{}", uuid_like()),
            name: name.into(),
            input: args,
        }
    }

    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        format!(
            "{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )
    }

    /// Scan a `CompletionRequest` for the *latest* task statuses the
    /// manager has rendered. The manager re-renders the state block on
    /// every tick, so later occurrences in the message history are
    /// newer. We walk the messages from most-recent to oldest and keep
    /// the first hit for each id.
    fn parse_state_from_request(req: &CompletionRequest) -> Vec<(String, String)> {
        use std::collections::HashMap;
        let mut latest: HashMap<String, String> = HashMap::new();
        for msg in req.messages.iter().rev() {
            for b in msg.content.iter() {
                let ContentBlock::Text { text } = b else {
                    continue;
                };
                for line in text.lines() {
                    let trimmed = line.trim_start();
                    let Some(rest) = trimmed.strip_prefix("- ") else {
                        continue;
                    };
                    let Some(id) = rest.split_whitespace().next() else {
                        continue;
                    };
                    if !id.starts_with('t') {
                        continue;
                    }
                    if latest.contains_key(id) {
                        continue; // already have the newer value
                    }
                    for piece in rest.split_whitespace() {
                        if matches!(
                            piece,
                            "Pending"
                                | "Todo"
                                | "InProgress"
                                | "Review"
                                | "Done"
                                | "Failed"
                                | "Blocked"
                        ) {
                            latest.insert(id.to_string(), piece.to_string());
                            break;
                        }
                    }
                }
            }
        }
        latest.into_iter().collect()
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn id(&self) -> &str {
            "scripted-test"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: false,
                tools: true,
                vision: false,
                cache_kind: wingman_core::CacheKind::None,
            }
        }
        async fn complete(
            &self,
            req: CompletionRequest,
        ) -> wingman_core::Result<ProviderEventStream> {
            use futures::stream;
            let mut n = self.call_count.lock().unwrap();
            *n += 1;
            let _call = *n;
            drop(n);

            // Inspect the user prompt to decide what to emit.
            let statuses = parse_state_from_request(&req);
            // For each task, classify Pending/Todo/Review/Done/etc.
            let task_status = |id: &str| -> Option<String> {
                statuses
                    .iter()
                    .filter(|(i, _)| i == id)
                    .map(|(_, s)| s.clone())
                    .next()
            };
            // Are all tasks Done?
            let all_done = ["t1", "t2", "t3"]
                .iter()
                .all(|id| task_status(id).as_deref() == Some("Done"));
            if all_done {
                // End the turn — nothing more to do.
                let events = vec![Ok(StreamEvent::Stop {
                    reason: StopReason::EndTurn,
                })];
                return Ok(Box::pin(stream::iter(events)));
            }

            // Pick the next move. Priority: finalize any Review, then
            // assign any Todo/Pending whose deps are Done.
            let blocks = if let Some(id) = ["t1", "t2", "t3"]
                .iter()
                .find(|id| task_status(id).as_deref() == Some("Review"))
            {
                tool_use(
                    "finalize_task",
                    serde_json::json!({
                        "task_id": id,
                        "merge_commit": format!("sha-{id}")
                    }),
                )
            } else if task_status("t1").as_deref() != Some("Done")
                && matches!(
                    task_status("t1").as_deref(),
                    Some("Pending") | Some("Todo") | Some("Failed")
                )
            {
                tool_use("assign_task", serde_json::json!({"task_id": "t1"}))
            } else if task_status("t2").as_deref() != Some("Done")
                && task_status("t1").as_deref() == Some("Done")
                && matches!(
                    task_status("t2").as_deref(),
                    Some("Pending") | Some("Todo") | Some("Failed")
                )
            {
                tool_use("assign_task", serde_json::json!({"task_id": "t2"}))
            } else if task_status("t3").as_deref() != Some("Done")
                && task_status("t1").as_deref() == Some("Done")
                && task_status("t2").as_deref() == Some("Done")
                && matches!(
                    task_status("t3").as_deref(),
                    Some("Pending") | Some("Todo") | Some("Failed")
                )
            {
                tool_use("assign_task", serde_json::json!({"task_id": "t3"}))
            } else {
                // Nothing actionable — end the turn (manager will be
                // re-invoked next tick with updated state).
                let events = vec![Ok(StreamEvent::Stop {
                    reason: StopReason::EndTurn,
                })];
                return Ok(Box::pin(stream::iter(events)));
            };

            let events = vec![
                Ok(StreamEvent::ToolUse { block: blocks }),
                Ok(StreamEvent::Usage {
                    usage: Usage::default(),
                }),
                Ok(StreamEvent::Stop {
                    reason: StopReason::ToolUse,
                }),
            ];
            Ok(Box::pin(stream::iter(events)))
        }
    }

    /// Mock CommandRunner that simulates a clean gh-present, git-push-ok
    /// environment. Useful for the e2e test below.
    struct AllOkCommandRunner;
    impl CommandRunner for AllOkCommandRunner {
        fn run(&self, program: &str, args: &[&str], _cwd: &Path) -> std::io::Result<CommandOut> {
            let _ = (program, args);
            // gh pr create's URL should be on stdout when program == gh and
            // first arg is "pr".
            let stdout = if program == "gh" && args.first().copied() == Some("pr") {
                "https://github.com/test/repo/pull/1\n".to_string()
            } else {
                String::new()
            };
            Ok(CommandOut {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        }
    }

    /// Runner where every command fails (exit 1) — used to simulate a host
    /// with no `docker` daemon for the J11 degradation test.
    struct AllFailRunner;
    impl CommandRunner for AllFailRunner {
        fn run(&self, _program: &str, _args: &[&str], _cwd: &Path) -> std::io::Result<CommandOut> {
            Ok(CommandOut {
                status: Some(1),
                stdout: String::new(),
                stderr: "not found".into(),
            })
        }
    }

    /// Phase 8.6 acceptance: end-to-end pipeline against a stub provider
    /// and fake spawner.
    ///
    /// The scripted provider emits the right tool calls so the manager
    /// loop schedules t1 → t2 → t3 with dep edges enforced. The fake
    /// spawner moves each task to Review. The manager calls finalize for
    /// each. After the manager loop, the pipeline detects need_merge,
    /// runs merge_integration on a real git repo we set up in tempdir,
    /// and opens a "PR" via the all-ok mock runner.
    ///
    /// This is the test the plan asked for in line 723: "a tiny scratch
    /// repo and a stubbed provider that returns canned tool calls."
    #[tokio::test]
    async fn pipeline_drives_three_task_run_to_completion_with_stub_provider() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().to_path_buf();

        // Initialise a real git repo so worktree::merge_integration has
        // something to work with.
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&project_root)
            .arg("init")
            .arg("--initial-branch=main")
            .output();
        // Some git versions don't support --initial-branch; fall through.
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&project_root)
            .arg("init")
            .output();
        std::fs::write(project_root.join("seed.txt"), b"seed\n").unwrap();
        for args in [vec!["add", "-A"], vec!["commit", "-m", "seed"]] {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&project_root)
                .args(&args)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "t@t.t")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "t@t.t")
                .output()
                .unwrap();
        }
        let base = std::process::Command::new("git")
            .arg("-C")
            .arg(&project_root)
            .arg("rev-parse")
            .arg("HEAD")
            .output()
            .unwrap();
        let base_commit = String::from_utf8_lossy(&base.stdout).trim().to_string();

        let run_id = "e2e";
        let run_dir = crate::run_dir(&project_root, run_id);
        let mut store = RunStore::create(
            &run_dir,
            run_id,
            "demo goal",
            &base_commit,
            &crate::integration_branch(run_id),
        )
        .await
        .unwrap();

        // Pre-seed the plan.
        for (id, deps) in [("t1", vec![]), ("t2", vec!["t1"]), ("t3", vec!["t1", "t2"])] {
            store
                .append(Event::TaskCreate {
                    t: RunStore::now(),
                    id: id.into(),
                    role: Role::Developer,
                    title: format!("Task {id}"),
                    goal: String::new(),
                    deps: deps.into_iter().map(String::from).collect(),
                    writes: vec![format!("{id}.txt")],
                    acceptance: vec![],
                    reversibility: Default::default(),
                    reversibility_reason: None,
                })
                .await
                .unwrap();
        }

        // Drive the pipeline. use_real_worktrees=true so the merge step
        // has actual branches to merge. The fake_happy_spawner doesn't
        // know about real worktrees — the orchestrator creates them and
        // then the spawner emits events. For the merge to actually
        // produce a diff, we'd need the worker to commit; the fake
        // spawner doesn't write files, so the squash-merge will produce
        // empty commits. merge_integration uses --allow-empty so this is
        // fine for this test.
        let inputs = PipelineInputs {
            provider: Arc::new(ScriptedProvider::new()),
            manager_model: "stub".into(),
            worker_spawner: fake_happy_spawner(),
            base_branch: "main".into(),
            project_root: project_root.clone(),
            command_runner: Box::new(AllOkCommandRunner),
            no_pr: false,
            orchestrator_cfg: OrchestratorConfig {
                max_concurrent_agents: 4,
                task_timeout: std::time::Duration::from_secs(30),
                project_root: project_root.clone(),
                run_id: run_id.into(),
                base_commit: base_commit.clone(),
                use_real_worktrees: true,
                max_usd: 0.0,
                max_retries_per_task: 0,
                enforce_checkpoint_hygiene: false,
            },
            max_ticks: 32,
            tier: wingman_config::PilotTier::Copilot,
            worker_model: "stub-worker".into(),
            stats_path: None,
            auto_approved: false,
            pr_config: wingman_config::PilotPrConfig::default(),
            security_config: wingman_config::PilotSecurityConfig::default(),
            run_reviewer: false,
            run_critic: false,
            reviewer_model: "stub".into(),
            sandbox_default_tier: "host".into(),
            dangerous_paths: Vec::new(),
        };

        let outcome = run_to_completion(store, inputs).await.unwrap();
        assert!(
            outcome.failed_tasks.is_empty(),
            "tasks ended Failed: {:?}",
            outcome.failed_tasks
        );
        // The scripted provider calls finalize_task per task, so the
        // pipeline's end-of-run merge step is a no-op and `merged` is
        // None — that's the incremental-finalize flow. The deferred-
        // finalize flow (where the manager skips finalize_task and the
        // pipeline merges everything at the end) is exercised by
        // worktree::tests::three_task_run_produces_three_squashed_commits.
        // Either way the PR must be opened.
        let pr = outcome.pr.expect("PR step ran");
        assert!(pr.created_by_gh, "all-ok runner should pick the gh path");
        assert!(pr.url.contains("github.com/test/repo/pull/1"));

        // Final state: every task Done, run.pr + run.done in log.
        let final_store = RunStore::load(&run_dir).await.unwrap();
        let state = final_store.state();
        for id in ["t1", "t2", "t3"] {
            assert_eq!(
                state.task(id).map(|t| t.status),
                Some(TaskStatus::Done),
                "task {id} not Done"
            );
        }
        assert_eq!(
            state.pr_url.as_deref(),
            Some("https://github.com/test/repo/pull/1")
        );
        assert!(matches!(state.status, RunStatus::Done));
    }

    /// R3 wiring: a blocked run writes an escalation packet naming the
    /// blocked task and a resume command.
    #[tokio::test]
    async fn blocked_run_writes_escalation_packet() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().to_path_buf();
        let run_id = "blocked-run";

        let mut state = crate::model::RunState::new(
            run_id,
            "do something risky",
            "abc123",
            crate::integration_branch(run_id),
        );
        let mut t1 = Task::new("t1", Role::Developer, "the hard part");
        t1.status = TaskStatus::Blocked;
        state.tasks.push(t1);

        let path = write_escalation_packet(
            &project_root,
            run_id,
            &state,
            wingman_config::PilotTier::Copilot,
            &[],
        )
        .expect("packet written");

        assert!(path.ends_with("escalation.md"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# Escalation: blocked-run"));
        assert!(body.contains("blocked at task #t1"));
        assert!(body.contains("the hard part"));
        assert!(body.contains("wingman pilot resume blocked-run"));
    }

    /// R3 wiring: detected J15 triggers are rendered into the packet's
    /// "Escalation triggers" section, so the human page names the reason.
    #[tokio::test]
    async fn blocked_run_packet_lists_escalation_triggers() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().to_path_buf();
        let run_id = "triggered-run";

        let mut state = crate::model::RunState::new(
            run_id,
            "tidy up the codebase",
            "abc123",
            crate::integration_branch(run_id),
        );
        // A task that wrote a dangerous path the goal never mentioned.
        let mut t1 = Task::new("t1", Role::Developer, "touch auth");
        t1.status = TaskStatus::Blocked;
        t1.writes = vec!["crates/auth/src/token.rs".to_string()];
        state.tasks.push(t1);

        let triggers = detect_escalation_triggers(
            &RecordingRunner::new(),
            &project_root,
            &state.base_commit,
            &crate::integration_branch(run_id),
            &state,
            &["**/auth/**".to_string()],
        );
        assert!(
            !triggers.is_empty(),
            "expected a dangerous-path trigger from the auth write"
        );

        let path = write_escalation_packet(
            &project_root,
            run_id,
            &state,
            wingman_config::PilotTier::Copilot,
            &triggers,
        )
        .expect("packet written");

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("## Escalation triggers"));
        assert!(body.to_lowercase().contains("auth"));
    }

    /// Recording CommandRunner: captures every invocation; all succeed.
    struct RecordingRunner {
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }
    impl RecordingRunner {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }
    impl CommandRunner for RecordingRunner {
        fn run(&self, program: &str, args: &[&str], _cwd: &Path) -> std::io::Result<CommandOut> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
            let stdout = if program == "gh"
                && args.first().copied() == Some("pr")
                && args.get(1).copied() == Some("create")
            {
                "https://github.com/test/repo/pull/9\n".to_string()
            } else {
                String::new()
            };
            Ok(CommandOut {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn e6_record_run_stats_writes_one_per_task() {
        let dir = tempdir().unwrap();
        let stats = dir.path().join("stats.jsonl");
        let mut state = crate::model::RunState::new("r1", "the goal", "abc", "b");
        let mut t1 = Task::new("t1", Role::Developer, "x");
        t1.status = TaskStatus::Done;
        let mut t2 = Task::new("t2", Role::Tester, "y");
        t2.status = TaskStatus::Done;
        state.tasks = vec![t1, t2];

        record_run_stats(&stats, &state, "haiku");

        let loaded = crate::learning::load_stats(&stats).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().all(|r| r.first_try_ok && r.model == "haiku"));
        assert!(loaded.iter().any(|r| r.role == "developer"));
        assert!(loaded.iter().any(|r| r.role == "tester"));
    }

    #[test]
    fn e8_auto_merge_holds_when_not_auto_approved() {
        let runner = RecordingRunner::new();
        let pr = PrOutcome {
            url: "https://x/pull/1".into(),
            created_by_gh: true,
        };
        let decision = decide_and_maybe_merge(
            &runner,
            Path::new("."),
            &wingman_config::PilotPrConfig {
                auto_merge: true,
                require_ci_green: false,
                ..Default::default()
            },
            false, // not auto-approved
            wingman_config::PilotTier::Copilot,
            false, // security clean
            true,  // reviewed
            None,  // no review findings
            false, // no critic veto
            false, // no dangerous paths (J15)
            &pr,
        );
        assert!(!decision.is_merge());
        // No gh pr merge call.
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn e8_auto_merge_fires_when_trusted_and_ci_not_required() {
        let runner = RecordingRunner::new();
        let pr = PrOutcome {
            url: "https://x/pull/1".into(),
            created_by_gh: true,
        };
        let decision = decide_and_maybe_merge(
            &runner,
            Path::new("."),
            &wingman_config::PilotPrConfig {
                auto_merge: true,
                require_ci_green: false,
                auto_merge_max_severity: "low".into(),
                base_branch: "main".into(),
                reviewer_rework_severity: "high".into(),
            },
            true, // auto-approved
            wingman_config::PilotTier::Copilot,
            false, // security clean
            true,  // reviewed
            None,  // no review findings
            false, // no critic veto
            false, // no dangerous paths (J15)
            &pr,
        );
        assert!(decision.is_merge());
        let calls = runner.calls.lock().unwrap();
        assert!(
            calls.iter().any(|(p, a)| p == "gh"
                && a.first().map(|s| s.as_str()) == Some("pr")
                && a.get(1).map(|s| s.as_str()) == Some("merge")),
            "expected a gh pr merge call, got {calls:?}"
        );
    }

    /// Runner that returns a canned `gh pr checks --json state` body and an
    /// all-ok `gh pr create`/`gh pr merge`. Used to drive the CI gate.
    struct CiRunner {
        checks_json: String,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }
    impl CiRunner {
        fn new(checks_json: &str) -> Self {
            Self {
                checks_json: checks_json.to_string(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }
    impl CommandRunner for CiRunner {
        fn run(&self, program: &str, args: &[&str], _cwd: &Path) -> std::io::Result<CommandOut> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
            let is_checks = program == "gh"
                && args.first().copied() == Some("pr")
                && args.get(1).copied() == Some("checks");
            let stdout = if is_checks {
                self.checks_json.clone()
            } else {
                String::new()
            };
            Ok(CommandOut {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn query_ci_status_all_success_is_green() {
        let runner = CiRunner::new(r#"[{"state":"SUCCESS"},{"state":"SKIPPED"}]"#);
        assert_eq!(
            query_ci_status(&runner, Path::new("."), "https://x/pull/1"),
            Some(true)
        );
    }

    #[test]
    fn query_ci_status_any_failure_is_red() {
        let runner = CiRunner::new(r#"[{"state":"SUCCESS"},{"state":"FAILURE"}]"#);
        assert_eq!(
            query_ci_status(&runner, Path::new("."), "https://x/pull/1"),
            Some(false)
        );
    }

    #[test]
    fn query_ci_status_pending_is_unknown() {
        let runner = CiRunner::new(r#"[{"state":"SUCCESS"},{"state":"IN_PROGRESS"}]"#);
        assert_eq!(
            query_ci_status(&runner, Path::new("."), "https://x/pull/1"),
            None
        );
    }

    #[test]
    fn query_ci_status_no_checks_is_unknown() {
        let runner = CiRunner::new("[]");
        assert_eq!(
            query_ci_status(&runner, Path::new("."), "https://x/pull/1"),
            None
        );
    }

    #[test]
    fn e8_auto_merge_fires_when_ci_required_and_green() {
        let runner = CiRunner::new(r#"[{"state":"SUCCESS"}]"#);
        let pr = PrOutcome {
            url: "https://x/pull/1".into(),
            created_by_gh: true,
        };
        let decision = decide_and_maybe_merge(
            &runner,
            Path::new("."),
            &wingman_config::PilotPrConfig {
                auto_merge: true,
                require_ci_green: true,
                auto_merge_max_severity: "low".into(),
                base_branch: "main".into(),
                reviewer_rework_severity: "high".into(),
            },
            true,
            wingman_config::PilotTier::Copilot,
            false,
            true, // reviewed
            None,
            false,
            false, // no dangerous paths (J15)
            &pr,
        );
        assert!(decision.is_merge());
        let calls = runner.calls.lock().unwrap();
        assert!(calls.iter().any(|(p, a)| p == "gh"
            && a.first().map(|s| s.as_str()) == Some("pr")
            && a.get(1).map(|s| s.as_str()) == Some("checks")));
        assert!(calls
            .iter()
            .any(|(p, a)| p == "gh" && a.get(1).map(|s| s.as_str()) == Some("merge")));
    }

    #[test]
    fn e8_auto_merge_holds_when_ci_required_and_red() {
        let runner = CiRunner::new(r#"[{"state":"FAILURE"}]"#);
        let pr = PrOutcome {
            url: "https://x/pull/1".into(),
            created_by_gh: true,
        };
        let decision = decide_and_maybe_merge(
            &runner,
            Path::new("."),
            &wingman_config::PilotPrConfig {
                auto_merge: true,
                require_ci_green: true,
                auto_merge_max_severity: "low".into(),
                base_branch: "main".into(),
                reviewer_rework_severity: "high".into(),
            },
            true,
            wingman_config::PilotTier::Copilot,
            false,
            true, // reviewed
            None,
            false,
            false, // no dangerous paths (J15)
            &pr,
        );
        assert!(!decision.is_merge());
        assert!(!runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(p, a)| p == "gh" && a.get(1).map(|s| s.as_str()) == Some("merge")));
    }

    #[test]
    fn e8_security_block_holds_even_when_auto_approved() {
        let runner = RecordingRunner::new();
        let pr = PrOutcome {
            url: "https://x/pull/1".into(),
            created_by_gh: true,
        };
        let decision = decide_and_maybe_merge(
            &runner,
            Path::new("."),
            &wingman_config::PilotPrConfig {
                auto_merge: true,
                require_ci_green: false,
                ..Default::default()
            },
            true, // auto-approved
            wingman_config::PilotTier::Copilot,
            true, // security pass blocks
            true, // reviewed
            None,
            false,
            false, // no dangerous paths (J15)
            &pr,
        );
        assert!(!decision.is_merge());
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn r6_security_pass_flags_secret_in_diff() {
        struct DiffRunner;
        impl CommandRunner for DiffRunner {
            fn run(
                &self,
                program: &str,
                args: &[&str],
                _cwd: &Path,
            ) -> std::io::Result<CommandOut> {
                let stdout = if program == "git" && args.first().copied() == Some("diff") {
                    "+++ b/config.rs\n+let key = \"AKIAIOSFODNN7EXAMPLE\";\n".to_string()
                } else {
                    String::new()
                };
                Ok(CommandOut {
                    status: Some(0),
                    stdout,
                    stderr: String::new(),
                })
            }
        }
        let report = run_security_pass(
            &DiffRunner,
            Path::new("."),
            "base123",
            "wingman/auto/r1",
            &wingman_config::PilotSecurityConfig::default(),
        );
        assert!(!report.findings.is_empty());
        assert!(report.blocks_merge(crate::severity::Severity::Medium));
    }

    #[test]
    fn r6_security_pass_clean_diff_is_empty() {
        struct DiffRunner;
        impl CommandRunner for DiffRunner {
            fn run(
                &self,
                program: &str,
                args: &[&str],
                _cwd: &Path,
            ) -> std::io::Result<CommandOut> {
                let stdout = if program == "git" && args.first().copied() == Some("diff") {
                    "+++ b/main.rs\n+let total = items.len();\n".to_string()
                } else {
                    String::new()
                };
                Ok(CommandOut {
                    status: Some(0),
                    stdout,
                    stderr: String::new(),
                })
            }
        }
        let report = run_security_pass(
            &DiffRunner,
            Path::new("."),
            "base123",
            "wingman/auto/r1",
            &wingman_config::PilotSecurityConfig::default(),
        );
        assert!(report.findings.is_empty());
    }

    /// Runner returning a diff that both leaks a secret and edits a license
    /// header — exercises the J15 diff-side detection.
    struct J15DiffRunner;
    impl CommandRunner for J15DiffRunner {
        fn run(&self, program: &str, args: &[&str], _cwd: &Path) -> std::io::Result<CommandOut> {
            let stdout = if program == "git" && args.first().copied() == Some("diff") {
                concat!(
                    "--- a/LICENSE\n+++ b/LICENSE\n",
                    "-Copyright 2025 Old Owner\n+Copyright 2026 New Owner\n",
                    "--- a/cfg.rs\n+++ b/cfg.rs\n",
                    "+let key = \"AKIAIOSFODNN7EXAMPLE\";\n",
                )
                .to_string()
            } else {
                String::new()
            };
            Ok(CommandOut {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn j15_detects_secret_and_license_and_dangerous_path() {
        let mut state =
            crate::model::RunState::new("r1", "speed up the parser", "base123", "wingman/auto/r1");
        // A write to a dangerous path the goal never mentioned.
        state.tasks.push({
            let mut t = Task::new("t1", Role::Developer, "edit auth");
            t.writes = vec!["crates/auth/src/login.rs".into()];
            t
        });
        let triggers = detect_escalation_triggers(
            &J15DiffRunner,
            Path::new("."),
            "base123",
            "wingman/auto/r1",
            &state,
            &["**/auth/**".to_string()],
        );
        use crate::escalation::EscalationTrigger as T;
        assert!(triggers
            .iter()
            .any(|t| matches!(t, T::SecretsDetected { .. })));
        assert!(triggers
            .iter()
            .any(|t| matches!(t, T::LicenseHeaderModified { .. })));
        assert!(triggers
            .iter()
            .any(|t| matches!(t, T::DangerousPathTouched { .. })));
        assert!(triggers.iter().any(|t| t.blocks_auto_merge()));
    }

    #[test]
    fn j15_quiet_when_goal_mentions_path_and_diff_clean() {
        struct CleanRunner;
        impl CommandRunner for CleanRunner {
            fn run(
                &self,
                program: &str,
                args: &[&str],
                _cwd: &Path,
            ) -> std::io::Result<CommandOut> {
                let stdout = if program == "git" && args.first().copied() == Some("diff") {
                    "+++ b/auth.rs\n+let total = items.len();\n".to_string()
                } else {
                    String::new()
                };
                Ok(CommandOut {
                    status: Some(0),
                    stdout,
                    stderr: String::new(),
                })
            }
        }
        let mut state = crate::model::RunState::new(
            "r1",
            "refactor the auth login flow",
            "base123",
            "wingman/auto/r1",
        );
        state.tasks.push({
            let mut t = Task::new("t1", Role::Developer, "edit auth");
            t.writes = vec!["crates/auth/src/login.rs".into()];
            t
        });
        let triggers = detect_escalation_triggers(
            &CleanRunner,
            Path::new("."),
            "base123",
            "wingman/auto/r1",
            &state,
            &["**/auth/**".to_string()],
        );
        assert!(
            triggers.is_empty(),
            "goal mentions auth + clean diff → no triggers, got {triggers:?}"
        );
    }

    /// Provider that returns a fixed text body for any request — stands in
    /// for a reviewer/critic agent emitting a JSON verdict.
    struct CannedTextProvider {
        text: String,
    }
    #[async_trait]
    impl Provider for CannedTextProvider {
        fn id(&self) -> &str {
            "canned-text"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: false,
                tools: false,
                vision: false,
                cache_kind: wingman_core::CacheKind::None,
            }
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> wingman_core::Result<ProviderEventStream> {
            use futures::stream;
            let events = vec![
                Ok(StreamEvent::TextDelta {
                    text: self.text.clone(),
                }),
                Ok(StreamEvent::Stop {
                    reason: StopReason::EndTurn,
                }),
            ];
            Ok(Box::pin(stream::iter(events)))
        }
    }

    fn done_task(id: &str) -> Task {
        let mut t = Task::new(id, Role::Developer, format!("task {id}"));
        t.status = TaskStatus::Done;
        t.outcome = Some(crate::model::TaskOutcome {
            summary: "did the thing".into(),
            files_changed: vec![format!("{id}.rs")],
        });
        t
    }

    #[test]
    fn j11_compute_sandbox_tiers_escalates_per_task() {
        let mut state = crate::model::RunState::new("r1", "g", "abc", "b");
        // Plain edit → stays host.
        state.tasks.push({
            let mut t = Task::new("t1", Role::Developer, "edit");
            t.writes = vec!["crates/cli/src/main.rs".into()];
            t
        });
        // Migration → vm.
        state.tasks.push({
            let mut t = Task::new("t2", Role::Developer, "migrate");
            t.writes = vec!["db/migrations/001.sql".into()];
            t
        });
        // No Docker → vm/container degrade to host. Use a runner that
        // reports docker absent so the test is deterministic.
        let no_docker = AllFailRunner;
        let tiers = compute_sandbox_tiers(&state, "host", &no_docker);
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers.iter().find(|(id, _)| id == "t1").unwrap().1, "host");
        // t2 selects vm but degrades to host without a daemon.
        assert_eq!(tiers.iter().find(|(id, _)| id == "t2").unwrap().1, "host");
    }

    #[test]
    fn j11_keeps_vm_tier_when_docker_present() {
        let mut state = crate::model::RunState::new("r1", "g", "abc", "b");
        state.tasks.push({
            let mut t = Task::new("t2", Role::Developer, "migrate");
            t.writes = vec!["db/migrations/001.sql".into()];
            t
        });
        let with_docker = AllOkCommandRunner;
        let tiers = compute_sandbox_tiers(&state, "host", &with_docker);
        assert_eq!(tiers[0].1, "vm");
    }

    #[test]
    fn j11_sandbox_default_is_a_floor() {
        let mut state = crate::model::RunState::new("r1", "g", "abc", "b");
        state.tasks.push(Task::new("t1", Role::Developer, "edit"));
        // Default container floor lifts even a plain task to container —
        // when Docker is available.
        let tiers = compute_sandbox_tiers(&state, "container", &AllOkCommandRunner);
        assert_eq!(tiers[0].1, "container");
    }

    #[tokio::test]
    async fn e7_inline_reviewer_rework_returns_notes() {
        let provider = CannedTextProvider {
            text: r#"{"verdict":"rework","summary":"needs tests","findings":[{"severity":"high","message":"no error handling"}]}"#.into(),
        };
        let task = done_task("t1");
        // High finding at the Medium gate → rework, with notes.
        let notes = review_task_inline(
            &provider,
            "m",
            &task,
            "--- a/x\n+++ b/x\n@@\n+bug",
            crate::severity::Severity::Medium,
        )
        .await;
        assert!(notes.is_some(), "rework verdict must return notes");
        assert!(notes.unwrap().contains("no error handling"));
    }

    #[tokio::test]
    async fn e7_inline_reviewer_defaults_to_approve_on_garbage() {
        let provider = CannedTextProvider {
            text: "not json at all".into(),
        };
        let task = done_task("t1");
        // Unparseable → fail-open approve → None (finalize proceeds).
        let notes = review_task_inline(
            &provider,
            "m",
            &task,
            "--- a/x\n+++ b/x\n@@\n+ok",
            crate::severity::Severity::Medium,
        )
        .await;
        assert!(notes.is_none(), "garbage must fail open to approve");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolver_bridge_resolves_conflict_via_block_in_place() {
        // Exercises the exact live path: the sync merge calls a resolver that
        // bridges to the async `resolve_conflicts_inline` via block_in_place +
        // block_on. Uses a canned provider so it's deterministic and offline.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().to_path_buf();
        let gitc = |dir: &std::path::Path, args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t.t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t.t")
                .output()
        };
        if gitc(&repo, &["init", "-q"]).is_err() {
            eprintln!("skipping: git not available");
            return;
        }
        // Persist identity + line-ending settings in the repo config: CI
        // runners have no global git identity, and `merge_integration`'s
        // internal rebase/merge shell out without the per-command env vars,
        // so they'd otherwise fail with "Committer identity unknown".
        for cfg in [
            ["config", "user.email", "t@t.t"],
            ["config", "user.name", "t"],
            ["config", "core.autocrlf", "false"],
            ["config", "core.eol", "lf"],
        ] {
            gitc(&repo, &cfg).unwrap();
        }
        std::fs::write(repo.join("shared.txt"), "base\n").unwrap();
        gitc(&repo, &["add", "-A"]).unwrap();
        gitc(&repo, &["commit", "-qm", "seed"]).unwrap();
        let base = String::from_utf8(gitc(&repo, &["rev-parse", "HEAD"]).unwrap().stdout)
            .unwrap()
            .trim()
            .to_string();

        let run_id = "bridge";
        let mut state = crate::model::RunState::new(run_id, "g", &base, "wingman/auto/bridge");
        for (id, body) in [("t1", "A"), ("t2", "B")] {
            let mut task = Task::new(id, Role::Developer, format!("edit {id}"));
            task.status = TaskStatus::Review;
            state.tasks.push(task);
            let wt = repo
                .join(".wingman")
                .join("worktrees")
                .join(format!("auto-{run_id}-{id}"));
            crate::worktree::create_worktree(&repo, &base, run_id, id, &wt).unwrap();
            std::fs::write(wt.join("shared.txt"), format!("base\n{body}\n")).unwrap();
            gitc(&wt, &["add", "-A"]).unwrap();
            gitc(&wt, &["commit", "-qm", "edit"]).unwrap();
        }

        // Canned model returns a clean merged file.
        let provider = CannedTextProvider {
            text: "base\nA\nB\n".into(),
        };
        let repo_c = repo.clone();
        let resolver = move |files: &[String]| -> bool {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(resolve_conflicts_inline(
                    &provider, "m", &repo_c, files,
                ))
            })
        };

        let outcome = crate::worktree::merge_integration_with_resolver(
            &repo,
            &base,
            "wingman/auto/bridge",
            &state,
            Some(&resolver),
        )
        .expect("resolver bridge should land the conflicting task");
        assert_eq!(outcome.commits.len(), 2);
        let merged = std::fs::read_to_string(repo.join("shared.txt")).unwrap();
        assert!(merged.contains('A') && merged.contains('B') && !merged.contains("<<<<<<<"));
    }

    #[test]
    fn strip_fence_unwraps_a_single_code_block() {
        assert_eq!(strip_fence("plain"), "plain");
        assert_eq!(strip_fence("```rust\nlet x = 1;\n```"), "let x = 1;");
        assert_eq!(strip_fence("```\nno lang\n```"), "no lang");
    }

    #[tokio::test]
    async fn resolve_conflicts_inline_writes_resolved_content() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("shared.txt");
        std::fs::write(&path, "top\n<<<<<<< HEAD\nA\n=======\nB\n>>>>>>> other\n").unwrap();
        // The model returns the clean merged file (wrapped in a fence to also
        // exercise strip_fence).
        let provider = CannedTextProvider {
            text: "```\ntop\nA\nB\n```".into(),
        };
        let ok = resolve_conflicts_inline(&provider, "m", tmp.path(), &["shared.txt".into()]).await;
        assert!(ok, "resolver should report success");
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("<<<<<<<") && after.contains('A') && after.contains('B'));
    }

    #[tokio::test]
    async fn resolve_conflicts_inline_rejects_a_still_conflicted_answer() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("shared.txt");
        let original = "<<<<<<< HEAD\nA\n=======\nB\n>>>>>>> other\n";
        std::fs::write(&path, original).unwrap();
        // Model hands back markers still present → reject, leave file untouched.
        let provider = CannedTextProvider {
            text: "<<<<<<< still\nA\n=======\nB\n>>>>>>> broken".into(),
        };
        let ok = resolve_conflicts_inline(&provider, "m", tmp.path(), &["shared.txt".into()]).await;
        assert!(!ok, "a still-conflicted answer must be rejected");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[tokio::test]
    async fn j10_critic_pass_vetoes_on_high_risk() {
        let provider = CannedTextProvider {
            text:
                r#"{"summary":"risky","risks":[{"severity":"high","description":"drops a table"}]}"#
                    .into(),
        };
        let mut state = crate::model::RunState::new("r1", "drop a column", "abc", "b");
        state.tasks = vec![done_task("t1")];
        assert!(run_critic_pass(&provider, "m", &state, &mut wingman_core::Usage::default()).await);
    }

    #[tokio::test]
    async fn j10_critic_pass_no_veto_on_low_risk() {
        let provider = CannedTextProvider {
            text: r#"{"summary":"fine","risks":[{"severity":"low","description":"nit"}]}"#.into(),
        };
        let mut state = crate::model::RunState::new("r1", "g", "abc", "b");
        state.tasks = vec![done_task("t1")];
        assert!(!run_critic_pass(&provider, "m", &state, &mut wingman_core::Usage::default()).await);
    }

    #[tokio::test]
    async fn e11_flags_multifile_task_without_checkpoint() {
        let dir = tempdir().unwrap();
        let run_id = "ckpt-run";
        let run_dir = crate::run_dir(dir.path(), run_id);
        let mut store = RunStore::create(
            &run_dir,
            run_id,
            "g",
            "abc",
            crate::integration_branch(run_id),
        )
        .await
        .unwrap();
        store
            .append(Event::TaskCreate {
                t: RunStore::now(),
                id: "t1".into(),
                role: Role::Developer,
                title: "edits two files".into(),
                goal: String::new(),
                deps: vec![],
                writes: vec!["a.rs".into(), "b.rs".into()],
                acceptance: vec![],
                reversibility: Default::default(),
                reversibility_reason: None,
            })
            .await
            .unwrap();
        // Two edits, no checkpoint between them.
        for _ in 0..2 {
            store
                .append(Event::TaskTool {
                    t: RunStore::now(),
                    id: "t1".into(),
                    agent: "a".into(),
                    tool: "edit_file".into(),
                    input_hash: None,
                    file: None,
                    ok: true,
                })
                .await
                .unwrap();
        }
        store
            .append(Event::TaskStatus {
                t: RunStore::now(),
                id: "t1".into(),
                status: TaskStatus::Done,
                outcome: None,
            })
            .await
            .unwrap();

        let state = store.state().clone();
        let violations = compute_checkpoint_violations(&run_dir, &state).await;
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].0, "t1");
    }

    // Sanity: parse_state_from_request actually reads task statuses.
    #[test]
    fn parse_state_extracts_task_statuses_from_user_prompt() {
        let req = CompletionRequest {
            model: "x".into(),
            system: None,
            messages: vec![Message {
                role: ApiRole::User,
                content: vec![ContentBlock::Text {
                    text: "- t1 [developer] Done (deps: ...)\n- t2 [developer] Review (deps: t1)\n- t3 [developer] Pending (deps: t1,t2)".into(),
                }],
            }],
            tools: vec![],
            max_tokens: 4096,
            temperature: None,
            cache_breakpoints: vec![],
        };
        let s = parse_state_from_request(&req);
        assert!(s.iter().any(|(id, st)| id == "t1" && st == "Done"));
        assert!(s.iter().any(|(id, st)| id == "t2" && st == "Review"));
        assert!(s.iter().any(|(id, st)| id == "t3" && st == "Pending"));
    }

    #[tokio::test]
    async fn e4_record_merge_fixer_task_captures_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = RunStore::create(
            dir.path().join(".wingman/autonomous/mf-run"),
            "mf-run",
            "g",
            "base",
            "wingman/auto/mf-run",
        )
        .await
        .unwrap();
        record_merge_fixer_task(&mut store, "t2", &["src/a.rs".into(), "src/b.rs".into()]).await;
        let t = store
            .state()
            .task("merge-fixer-t2")
            .expect("merge-fixer task recorded");
        assert_eq!(t.role, crate::model::Role::MergeFixer);
        assert_eq!(t.writes, vec!["src/a.rs".to_string(), "src/b.rs".to_string()]);
        assert!(t.goal.contains("t2") && t.goal.contains("src/a.rs"));
    }

    #[test]
    fn j8_discover_crate_modules_extracts_pub_mods() {
        let tmp = tempfile::tempdir().unwrap();
        let crates = tmp.path().join("crates");
        let src = crates.join("wingman-foo").join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("lib.rs"),
            "//! doc\npub mod alpha;\nmod private;\n  pub mod beta ;\npub mod alpha;\n",
        )
        .unwrap();
        // a non-crate file at the top level must be ignored
        std::fs::write(crates.join("README"), "x").unwrap();

        let got = discover_crate_modules(&crates);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "wingman-foo");
        // deduped, sorted, private `mod` excluded
        assert_eq!(got[0].1, vec!["alpha".to_string(), "beta".to_string()]);
        // renders without panicking and names the crate
        let md = crate::knowledge::render_architecture(&got);
        assert!(md.contains("wingman-foo") && md.contains("alpha"));
    }

    // Silence unused-import warnings if the test gates above ever skip.
    #[allow(dead_code)]
    fn _unused_imports() {
        let _: Option<Task> = None;
        let _: AgentEvent = AgentEvent::Stop {
            reason: AgentStop::EndTurn,
        };
    }
}
