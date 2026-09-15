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

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;
use wingman_core::Provider;

use crate::manager::{build_manager, build_manager_registry, drive_to_completion, run_succeeded};
use crate::model::{Event, RunStatus, TaskStatus};
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
    /// `[tools].disabled_tools`, applied to the manager's own registry.
    /// A standing "not this one, ever" should hold in the pilot too, which is
    /// the least supervised place it could otherwise be reached.
    pub disabled_tools: Vec<String>,
    /// E7 — run a per-task reviewer agent after the run. Off by default.
    pub run_reviewer: bool,
    /// J10 — the critic agent that runs before the auto-merge gate; `None`
    /// skips it. The CLI resolves it from `[pilot].critic_model`, so it can
    /// sit on another provider than the manager.
    pub critic: Option<AuxAgent>,
    /// Model the reviewer agent runs on (usually `default_model`).
    pub reviewer_model: String,
    /// J11 default sandbox tier ("host" | "container" | "vm"); per-task
    /// tiers are escalated from this floor by `sandbox::select_tier`.
    pub sandbox_default_tier: String,
    /// J11 which non-host tiers this machine can honour, probed once by the
    /// caller. The same answer drives the worker spawner, so the reported
    /// tiers are the ones the workers actually ran in.
    pub sandbox_availability: crate::sandbox::TierAvailability,
    /// J15 `[pilot.approval].dangerous_paths` globs. A write to one of these
    /// that the goal text never mentions raises a hard escalation trigger
    /// and blocks auto-merge. Empty disables the check.
    pub dangerous_paths: Vec<String>,
    /// E4 — on a merge conflict the one-shot resolver cannot clear, spawn
    /// merge-fixer workers (through `worker_spawner`) before blocking the run.
    pub merge_fixer: bool,
    /// J8 — run the knowledge-keeper agent after the PR opens. `None` keeps
    /// only the deterministic knowledge upkeep (module map, hotspots, one
    /// decision record per run).
    pub knowledge_keeper: Option<AuxAgent>,
}

/// The provider and model one of the pipeline's side agents runs on: the J8
/// knowledge-keeper (routed through the `summarize` task class, so usually the
/// fast model) or the J10 critic (`[pilot].critic_model`).
pub struct AuxAgent {
    pub provider: Arc<dyn Provider>,
    pub model: String,
}

/// E4 — merge-fixer workers spawned per conflict before the run blocks on it.
const MERGE_FIXER_ATTEMPTS: u32 = 2;

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
    /// J11 per-task sandbox tier the run used, as `(task_id, tier)`, after
    /// degrading tiers this machine cannot honour.
    pub sandbox_tiers: Vec<(String, String)>,
    /// J15 hard escalation triggers detected over the integration diff +
    /// plan (dangerous-path-without-goal-mention, secrets, license-header
    /// edits). Empty on a clean run. Any blocking trigger vetoes auto-merge.
    pub escalation_triggers: Vec<crate::escalation::EscalationTrigger>,
    /// R6 security pass: findings plus a note per external scanner saying
    /// whether it ran, was not installed, or failed. `None` when no PR step
    /// ran (`--no-pr` or a blocked run).
    pub security: Option<crate::security::SecurityReport>,
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
    mut store: RunStore,
    inputs: PipelineInputs,
) -> Result<PipelineOutcome, PipelineError> {
    let state_at_start = store.state().clone();
    let integration_branch = state_at_start.integration_branch.clone();
    let project_root = inputs.project_root.clone();
    let run_id = state_at_start.run_id.clone();
    // Captured before the cfg is moved into the orchestrator — J15's runtime
    // cost trigger needs the cap at run end.
    let max_usd = inputs.orchestrator_cfg.max_usd;

    // Planning and the approval gate are both behind us by the time the
    // pipeline is entered, so this is where the run starts executing. Without
    // this the run sat at `Planning` for its whole life: nothing emitted
    // `Running`, so `pilot watch`'s header and anything else keying off
    // `RunStatus` reported a run as still planning while its workers ran.
    store
        .append(Event::RunStatusEv {
            t: RunStore::now(),
            status: RunStatus::Running,
        })
        .await?;

    // Keep a handle to the provider for the E7 inline reviewer and the E4
    // conflict resolver — `build_manager` consumes the original Arc.
    let aux_provider = inputs.provider.clone();

    // E7 — build the inline reviewer that gates each task's finalize. When
    // set, `run_reviewer` runs the reviewer at the Review→Done choke point
    // (race-free vs the manager) and sends rework verdicts back through the
    // retry ladder, instead of a batched post-run pass.
    // Captured before `inputs.manager_model` is moved into build_manager, so
    // the manager phase's tokens can be priced below.
    let manager_model = inputs.manager_model.clone();
    // Verdicts recorded by the inline reviewer, keyed by task id. Feeds the
    // auto-merge gate below so its severity branch is live.
    let review_log: std::sync::Arc<std::sync::Mutex<Vec<(String, InlineReview)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
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
        let review_log_for_closure = review_log.clone();
        Some(std::sync::Arc::new(move |task: crate::model::Task| {
            let provider = provider.clone();
            let model = model.clone();
            let repo = repo.clone();
            let run_id = run_id_for_review.clone();
            let base = base_for_review.clone();
            let log = review_log_for_closure.clone();
            Box::pin(async move {
                // Review the task's real diff. With no diff to show (git error,
                // empty change), approve rather than reject a change the model
                // can't see — the old sight-unseen path rejected correct work
                // and deadlocked the run.
                let diff = crate::worktree::task_diff(&repo, &run_id, &task.id, &base)?;
                let (rework, outcome) =
                    review_task_inline(provider.as_ref(), &model, &task, &diff, gate).await;
                if let Some(o) = outcome {
                    log.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((task.id.clone(), o));
                }
                rework
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        }))
    } else {
        None
    };

    // The merge step after the orchestrator exits reuses the worker spawner
    // for E4's merge-fixer.
    let fixer_spawner = inputs.merge_fixer.then(|| inputs.worker_spawner.clone());

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
    // The process cwd only when it is inside the project: `pilot
    // validate-providers` drives a scratch repo from wherever it was started,
    // and the manager's read tools must not resolve against that directory.
    let cwd = std::env::current_dir()
        .ok()
        .filter(|d| d.starts_with(&project_root))
        .unwrap_or_else(|| project_root.clone());
    let registry = build_manager_registry(
        handle.clone(),
        cwd,
        project_root.clone(),
        &inputs.disabled_tools,
    );
    let mut agent = build_manager(inputs.provider, inputs.manager_model, registry, None);

    let manager_usage = match drive_to_completion(&mut agent, &handle, inputs.max_ticks).await {
        Ok(usage) => usage,
        Err(e) => {
            // A run that cannot make progress (dependency deadlock, tick
            // budget exhausted) is over. Say so, or it stays `Running`
            // forever and every consumer reads a dead run as a live one.
            // Shut the orchestrator down first so it is not still writing
            // when we reopen the store.
            handle.shutdown().await;
            let _ = join.await;
            if let Ok(mut s) = RunStore::load(&crate::run_dir(&project_root, &run_id)).await {
                let _ = s
                    .append(Event::RunStatusEv {
                        t: RunStore::now(),
                        status: RunStatus::Failed,
                    })
                    .await;
            }
            return Err(e.into());
        }
    };

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
        record_run_stats(
            stats_path,
            &final_state,
            &read_run_events(&run_dir).await,
            &inputs.worker_model,
        );
    }

    // J11 — the sandbox tier each task ran in: escalated from the configured
    // default by its writes/acceptance/reversibility, then degraded to what
    // this machine can honour.
    let sandbox_tiers = compute_sandbox_tiers(
        &final_state,
        &inputs.sandbox_default_tier,
        &inputs.sandbox_availability,
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
        let mut escalation_triggers = detect_escalation_triggers(
            inputs.command_runner.as_ref(),
            &project_root,
            &final_state.base_commit,
            &integration_branch,
            &final_state,
            &inputs.dangerous_paths,
        );
        merge_escalations(&mut escalation_triggers, &final_state.escalations);
        let packet = write_escalation_packet(
            &project_root,
            &run_id,
            &final_state,
            inputs.tier,
            &escalation_triggers,
            &read_run_events(&run_dir).await,
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
            security: None,
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

    // E4 in-run conflict resolver: bridge the sync merge path to the async
    // resolvers in `resolve_conflict` — a one-shot model rewrite of the
    // conflict markers, then merge-fixer workers. Runs on the multi-thread
    // runtime via `block_in_place`. If neither resolves, the merge falls back
    // to the blocked path (the `Conflict` arm below), so a bad resolution
    // never lands — the files' markers are re-checked before commit.
    let resolve_provider = aux_provider.clone();
    let resolve_model = inputs.reviewer_model.clone();
    let resolve_root = project_root.clone();
    let resolve_run = run_id.clone();
    let resolve_branch = integration_branch.clone();
    let resolver = move |task_id: &str, files: &[String]| -> bool {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(resolve_conflict(
                resolve_provider.as_ref(),
                &resolve_model,
                fixer_spawner.as_ref(),
                &resolve_root,
                &resolve_run,
                &resolve_branch,
                task_id,
                files,
            ))
        })
    };

    let merged = need_merge.then(|| {
        worktree::merge_integration_with_resolver(
            &project_root,
            &final_state.base_commit,
            &integration_branch,
            &final_state,
            Some(&resolver),
        )
    });
    // Loaded after the merge: the resolver appends to the run log through
    // store handles of its own.
    let mut store = RunStore::load(&run_dir).await?;

    let merge_outcome = if let Some(merged) = merged {
        match merged {
            Ok(outcome) => {
                pr::finalize_all_review_tasks(&mut store, &final_state, &outcome.commits).await?;
                Some(outcome)
            }
            Err(crate::worktree::WorktreeError::Conflict { task_id, files }) => {
                // E4: neither the one-shot resolver nor the merge-fixer
                // workers (when enabled) cleared the conflict. Make sure a
                // merge-fixer task records the conflicted files so the
                // conflict is structured, resumable work (a `pilot resume`
                // picks it up), then write the R3 escalation packet and return
                // a blocked outcome instead of a raw error.
                tracing::warn!(
                    target: "pilot::pipeline",
                    task = %task_id, files = ?files,
                    "merge conflict unresolved — blocking the run"
                );
                if store.state().task(&merge_fixer_id(&task_id)).is_none() {
                    record_merge_fixer_task(&mut store, &task_id, &files).await;
                }
                let blocked_state = store.state().clone();
                let packet = write_escalation_packet(
                    &project_root,
                    &run_id,
                    &blocked_state,
                    inputs.tier,
                    &blocked_state.escalations,
                    &store.read_events().await.unwrap_or_default(),
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
                    escalation_triggers: blocked_state.escalations.clone(),
                    security: None,
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
            security: None,
        });
    }

    let snapshot_for_pr = store.state().clone();
    let pr_outcome = match pr::open_pull_request(
        inputs.command_runner.as_ref(),
        &mut store,
        &project_root,
        &inputs.base_branch,
        &integration_branch,
        &snapshot_for_pr,
        None,
    )
    .await
    {
        Ok(outcome) => outcome,
        // J15 — the push needed a force-push outside `wingman/auto/*`. The
        // integration branch is left as built; the run blocks on the trigger
        // with a packet, as any other blocked run does.
        Err(pr::PrError::ForcePushRefused(trigger)) => {
            tracing::warn!(target: "pilot::pipeline", "{}", trigger.render());
            let _ = store
                .append(Event::Escalation {
                    t: RunStore::now(),
                    trigger,
                })
                .await;
            let blocked_state = store.state().clone();
            let packet = write_escalation_packet(
                &project_root,
                &run_id,
                &blocked_state,
                inputs.tier,
                &blocked_state.escalations,
                &store.read_events().await.unwrap_or_default(),
            );
            return Ok(PipelineOutcome {
                merged: merge_outcome,
                pr: None,
                failed_tasks: Vec::new(),
                escalation_packet: packet,
                auto_merge: None,
                checkpoint_violations,
                reviews: Vec::new(),
                critic_vetoed: false,
                sandbox_tiers,
                escalation_triggers: blocked_state.escalations,
                security: None,
            });
        }
        Err(e) => return Err(e.into()),
    };

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
    if pr_outcome.created_by_gh {
        post_security_comment(
            inputs.command_runner.as_ref(),
            &project_root,
            &pr_outcome.url,
            &crate::security::render_report(&security_report, sec_gate),
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
    // J15 — fold in the runtime triggers the orchestrator's escalation
    // watchdog recorded while the run was live (net-negative tests, cost
    // warn/halt, failure streaks, irreversible task ran), then re-check spend
    // and the irreversible task against the final state: the manager's own
    // tokens are only priced after the orchestrator stops.
    merge_escalations(&mut escalation_triggers, &snapshot_for_pr.escalations);
    merge_escalations(
        &mut escalation_triggers,
        &crate::escalation::check_runtime(&crate::escalation::RuntimeSignals {
            state: &snapshot_for_pr,
            task: snapshot_for_pr
                .tasks
                .iter()
                .find(|t| matches!(t.reversibility, crate::model::Reversibility::Irreversible)),
            tests_before: None,
            tests_after: None,
            max_usd,
            recent_run_outcomes: &[],
        }),
    );
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
    let recorded = review_log.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let reviews: Vec<(String, crate::review::Verdict)> = recorded
        .iter()
        .map(|(id, o)| (id.clone(), o.verdict))
        .collect();
    let review_max_severity: Option<crate::severity::Severity> =
        crate::severity::max_severity(&recorded, |(_, o)| {
            o.max_severity.unwrap_or(crate::severity::Severity::Low)
        });

    // J10 — critic pass (opt-in). A high+ risk vetoes auto-merge.
    let mut critic_usage = wingman_core::Usage::default();
    let critic_vetoed = match &inputs.critic {
        Some(critic) => {
            let vetoed = run_critic_pass(
                critic.provider.as_ref(),
                &critic.model,
                &snapshot_for_pr,
                &mut critic_usage,
            )
            .await;
            record_phase_usage(&mut store, "critic", &critic.model, &critic_usage).await;
            vetoed
        }
        None => false,
    };

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
        // Whether a review actually ran on this run's diffs — not merely
        // whether the reviewer was enabled. A run whose tasks were batch-
        // finalized (skipping the inline choke point) records no verdicts and
        // is therefore correctly reported as unreviewed.
        inputs.run_reviewer && !reviews.is_empty(),
        review_max_severity,
        critic_vetoed,
        dangerous_paths_touched,
        &pr_outcome,
    );

    // J8 — maintain the durable project knowledge layer now that the run
    // merged: hotspots from this run's log, the module map, and (with the
    // knowledge-keeper on) an agent-written architecture summary and
    // decisions. Best-effort; a knowledge write must never fail the run.
    let mut keeper_usage = wingman_core::Usage::default();
    maintain_knowledge(
        &project_root,
        &snapshot_for_pr,
        &store.read_events().await.unwrap_or_default(),
        inputs.knowledge_keeper.as_ref(),
        &mut keeper_usage,
    )
    .await;
    if let Some(keeper) = &inputs.knowledge_keeper {
        record_phase_usage(&mut store, "knowledge", &keeper.model, &keeper_usage).await;
    }

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
        security: Some(security_report),
    })
}

/// J11 — the per-task sandbox tier for the run, from the configured default
/// floor + each task's writes/acceptance/reversibility, resolved against what
/// this machine can honour. Pure.
fn compute_sandbox_tiers(
    state: &crate::model::RunState,
    default_tier: &str,
    avail: &crate::sandbox::TierAvailability,
) -> Vec<(String, String)> {
    let floor = crate::sandbox::IsolationTier::parse(default_tier);
    state
        .tasks
        .iter()
        .map(|t| {
            let requested = crate::sandbox::select_tier(t, floor);
            let effective = crate::sandbox::resolve_effective_tier(requested, avail).0;
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

/// R6 — run the security pass over the integration diff:
///
/// - the built-in secrets scan (prefix + entropy) over the added lines of
///   `git diff <base>..<integration>` — always;
/// - gitleaks over the run's commits, when it is on PATH;
/// - for each lockfile the run changed, a license check of the packages it
///   added, against `allowed_licenses` / `denied_licenses`;
/// - `cargo audit` for each changed `Cargo.lock`, when cargo-audit is
///   installed.
///
/// The project root has the integration branch checked out by now (see
/// [`worktree::merge_integration`]), so tools that read the tree see the
/// run's result. A missing or failing external tool never fails the pass; it
/// is recorded in [`crate::security::SecurityReport::notes`] instead.
fn run_security_pass(
    runner: &dyn CommandRunner,
    project_root: &std::path::Path,
    base_commit: &str,
    integration_branch: &str,
    cfg: &wingman_config::PilotSecurityConfig,
) -> crate::security::SecurityReport {
    let mut report = crate::security::SecurityReport::default();
    let diff = collect_diff_lines(runner, project_root, base_commit, integration_branch);
    report.extend(crate::security::scan_secrets(&diff.added));
    run_gitleaks(
        runner,
        project_root,
        base_commit,
        integration_branch,
        &cfg.secrets_scanner,
        &mut report,
    );
    let lockfiles = changed_lockfiles(&diff);
    if !cfg.allowed_licenses.is_empty() || !cfg.denied_licenses.is_empty() {
        for lockfile in &lockfiles {
            scan_lockfile_licenses(
                runner,
                project_root,
                base_commit,
                integration_branch,
                lockfile,
                cfg,
                &mut report,
            );
        }
    }
    if cfg.dependency_audit {
        for lockfile in lockfiles.iter().filter(|l| file_name(l) == "Cargo.lock") {
            run_cargo_audit(runner, project_root, lockfile, &mut report);
        }
    }
    report
}

/// Every lockfile the diff changed, deduplicated. Lockfiles and not
/// manifests: a dependency a run added is only real once it is resolved into
/// the lockfile, and that is also where the exact version lives.
fn changed_lockfiles(diff: &DiffLines) -> Vec<String> {
    let mut files: Vec<String> = diff
        .changed
        .iter()
        .map(|(f, _)| f.clone())
        .filter(|f| matches!(file_name(f), "Cargo.lock" | "package-lock.json"))
        .collect();
    files.sort();
    files.dedup();
    files
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The directory a repo-relative lockfile lives in, for tools that read it
/// from their working directory.
fn lockfile_dir(project_root: &std::path::Path, lockfile: &str) -> PathBuf {
    match lockfile.rsplit_once('/') {
        Some((dir, _)) => project_root.join(dir),
        None => project_root.to_path_buf(),
    }
}

/// `git show <rev>:<path>`, or `None` when the file does not exist at `rev`.
fn git_show(
    runner: &dyn CommandRunner,
    project_root: &std::path::Path,
    rev: &str,
    path: &str,
) -> Option<String> {
    match runner.run("git", &["show", &format!("{rev}:{path}")], project_root) {
        Ok(o) if o.success() => Some(o.stdout),
        _ => None,
    }
}

/// License-check the packages `lockfile` gained between `base_commit` and
/// the integration branch. Packages already present at the base were
/// accepted before this run and are not re-litigated; a version bump counts
/// as new, because a new version can change its license.
fn scan_lockfile_licenses(
    runner: &dyn CommandRunner,
    project_root: &std::path::Path,
    base_commit: &str,
    integration_branch: &str,
    lockfile: &str,
    cfg: &wingman_config::PilotSecurityConfig,
    report: &mut crate::security::SecurityReport,
) {
    use crate::security::{parse_cargo_lock, parse_package_lock};
    // Absent at the base means every package is new; absent at the tip means
    // the run deleted the lockfile and added nothing.
    let before = git_show(runner, project_root, base_commit, lockfile).unwrap_or_default();
    let Some(after) = git_show(runner, project_root, integration_branch, lockfile) else {
        return;
    };
    let mut deps: Vec<(String, String)> = if file_name(lockfile) == "Cargo.lock" {
        let added: BTreeSet<(String, String)> = parse_cargo_lock(&after)
            .difference(&parse_cargo_lock(&before))
            .cloned()
            .collect();
        if added.is_empty() {
            return;
        }
        let Some(known) =
            collect_dependency_licenses(runner, &lockfile_dir(project_root, lockfile))
        else {
            report.notes.push(format!(
                "license scan of `{lockfile}` skipped: `cargo metadata` failed, so the licenses \
                 of {} new crate(s) are unchecked",
                added.len()
            ));
            return;
        };
        let deps: Vec<(String, String)> = known
            .into_iter()
            .filter(|(name, version, _)| added.contains(&(name.clone(), version.clone())))
            .map(|(name, _, license)| (name, license))
            .collect();
        if deps.len() < added.len() {
            report.notes.push(format!(
                "license scan of `{lockfile}`: {} new crate(s) missing from `cargo metadata` \
                 are unchecked",
                added.len() - deps.len()
            ));
        }
        deps
    } else {
        let before: BTreeSet<(String, String)> = parse_package_lock(&before)
            .unwrap_or_default()
            .into_iter()
            .map(|(name, version, _)| (name, version))
            .collect();
        match parse_package_lock(&after) {
            Ok(pkgs) => pkgs
                .into_iter()
                .filter(|(name, version, _)| !before.contains(&(name.clone(), version.clone())))
                .map(|(name, _, license)| (name, license))
                .collect(),
            Err(e) => {
                report
                    .notes
                    .push(format!("license scan of `{lockfile}` skipped: {e}"));
                return;
            }
        }
    };
    // npm installs one package at several paths; one finding each is enough.
    deps.sort();
    deps.dedup();
    report.notes.push(format!(
        "license scan: {} new package(s) in `{lockfile}` checked",
        deps.len()
    ));
    report.extend(crate::security::scan_licenses(
        &deps,
        lockfile,
        &cfg.allowed_licenses,
        &cfg.denied_licenses,
    ));
}

/// Collect `(name, version, spdx_license)` for every package `cargo metadata`
/// resolves in `dir`. `None` when cargo is missing or the command fails, so
/// the caller can say the licenses went unchecked rather than report a clean
/// scan. `--locked`: `dir` is the user's checkout, and a lockfile that does not
/// match its manifests is reported as unchecked rather than rewritten there.
fn collect_dependency_licenses(
    runner: &dyn CommandRunner,
    dir: &std::path::Path,
) -> Option<Vec<(String, String, String)>> {
    let args = ["metadata", "--format-version", "1", "--locked"];
    let out = match runner.run("cargo", &args, dir) {
        Ok(o) if o.success() => o.stdout,
        _ => return None,
    };
    let json: serde_json::Value = serde_json::from_str(&out).ok()?;
    let pkgs = json.get("packages")?.as_array()?;
    Some(
        pkgs.iter()
            .filter_map(|p| {
                let s = |k: &str| p.get(k).and_then(|v| v.as_str()).map(str::to_string);
                Some((
                    s("name")?,
                    s("version").unwrap_or_default(),
                    s("license").unwrap_or_default(),
                ))
            })
            .collect(),
    )
}

/// Run gitleaks over the commits between `base_commit` and the integration
/// branch, folding its findings into `report`. Only gitleaks' CLI is known
/// here, so any other `secrets_scanner` is noted as unsupported rather than
/// invoked with guessed arguments.
fn run_gitleaks(
    runner: &dyn CommandRunner,
    project_root: &std::path::Path,
    base_commit: &str,
    integration_branch: &str,
    scanner: &str,
    report: &mut crate::security::SecurityReport,
) {
    if scanner.is_empty() {
        return;
    }
    let is_gitleaks = std::path::Path::new(scanner)
        .file_stem()
        .is_some_and(|s| s.eq_ignore_ascii_case("gitleaks"));
    if !is_gitleaks {
        report.notes.push(format!(
            "secrets_scanner `{scanner}` is not supported (only gitleaks is); external secrets \
             scan skipped"
        ));
        return;
    }
    if base_commit.is_empty() {
        report
            .notes
            .push("gitleaks skipped: the run recorded no base commit to scan from".into());
        return;
    }
    if !runner
        .run(scanner, &["version"], project_root)
        .is_ok_and(|o| o.success())
    {
        report.notes.push(format!(
            "`{scanner}` not found on PATH; external secrets scan skipped (the built-in scan \
             still ran)"
        ));
        return;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let report_path = std::env::temp_dir().join(format!(
        "wingman-gitleaks-{}-{nanos}.json",
        std::process::id()
    ));
    let report_arg = report_path.to_string_lossy().into_owned();
    let range = format!("{base_commit}..{integration_branch}");
    // `--exit-code 0`: findings come from the report, so a leak is not an
    // error exit. `--redact` keeps the secret out of the report file.
    let out = runner.run(
        scanner,
        &[
            "detect",
            "--source",
            ".",
            "--log-opts",
            &range,
            "--report-format",
            "json",
            "--report-path",
            &report_arg,
            "--redact",
            "--no-banner",
            "--exit-code",
            "0",
        ],
        project_root,
    );
    let parsed = match out {
        Ok(o) if o.success() => std::fs::read_to_string(&report_path)
            .map_err(|e| format!("could not read its report: {e}"))
            .and_then(|json| crate::security::parse_gitleaks_report(&json)),
        Ok(o) => Err(format!(
            "exit {}: {}",
            o.status
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into()),
            o.stderr.lines().next().unwrap_or("").trim()
        )),
        Err(e) => Err(e.to_string()),
    };
    let _ = std::fs::remove_file(&report_path);
    match parsed {
        Ok(findings) => {
            report.notes.push(format!(
                "gitleaks: {} finding(s) over `{range}`",
                findings.len()
            ));
            report.extend(findings);
        }
        Err(e) => report.notes.push(format!(
            "gitleaks failed, external secrets scan incomplete: {e}"
        )),
    }
}

/// Run `cargo audit --json` next to a changed `Cargo.lock`. cargo-audit
/// exits non-zero when it finds advisories but still prints the JSON, so
/// stdout is parsed whatever the exit status; only unparseable output counts
/// as a failure.
fn run_cargo_audit(
    runner: &dyn CommandRunner,
    project_root: &std::path::Path,
    lockfile: &str,
    report: &mut crate::security::SecurityReport,
) {
    let out = match runner.run(
        "cargo",
        &["audit", "--json"],
        &lockfile_dir(project_root, lockfile),
    ) {
        Ok(o) => o,
        Err(e) => {
            report
                .notes
                .push(format!("cargo audit of `{lockfile}` skipped: {e}"));
            return;
        }
    };
    match crate::security::parse_cargo_audit(&out.stdout) {
        Ok(findings) => {
            report.notes.push(format!(
                "cargo audit: {} advisory(ies) in `{lockfile}`",
                findings.len()
            ));
            report.extend(findings);
        }
        Err(_) if out.stderr.contains("no such command") => report.notes.push(format!(
            "cargo-audit is not installed; dependency audit of `{lockfile}` skipped"
        )),
        Err(e) => {
            let detail = out
                .stderr
                .lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
                .unwrap_or(e);
            report
                .notes
                .push(format!("cargo audit of `{lockfile}` failed: {detail}"));
        }
    }
}

/// The body travels as a `gh` argument, and a Windows command line is capped
/// at 32767 characters (GitHub's own limit, 65536, is the looser of the two).
const MAX_PR_COMMENT_BYTES: usize = 30_000;

/// R6 — post the security summary on the PR `gh` opened, so it sits where
/// the change is reviewed. Best-effort: a failed comment is logged and the
/// run carries on, since the same report still gates auto-merge.
fn post_security_comment(
    runner: &dyn CommandRunner,
    project_root: &std::path::Path,
    pr_url: &str,
    summary: &str,
) {
    let mut body = summary.to_string();
    if body.len() > MAX_PR_COMMENT_BYTES {
        let mut cut = MAX_PR_COMMENT_BYTES;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
        body.push_str("\n\n… (truncated)\n");
    }
    match runner.run(
        "gh",
        &["pr", "comment", pr_url, "--body", &body],
        project_root,
    ) {
        Ok(o) if o.success() => {
            tracing::info!(target: "pilot::pipeline", url = %pr_url, "posted security summary")
        }
        Ok(o) => {
            tracing::warn!(target: "pilot::pipeline", stderr = %o.stderr, "gh pr comment failed")
        }
        Err(e) => tracing::warn!(target: "pilot::pipeline", error = %e, "gh pr comment errored"),
    }
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
    use futures::StreamExt;
    use wingman_core::{
        CacheBreakpoint, CompletionRequest, ContentBlock, Message, Role as ApiRole, StreamEvent,
        Usage,
    };
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
        // These are structured-output helper calls (review verdict, task plan),
        // not the agent loop. Reasoning would add cost and latency for output
        // that has to parse as JSON either way.
        reasoning: wingman_core::ReasoningEffort::Off,
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

/// E4 — resolve a conflict `merge_integration_with_resolver` hit while
/// squashing `task_id`: record it in the run log, try a one-shot model rewrite
/// of the conflict markers, then (when `fixer` is set) merge-fixer workers.
/// `true` once the integration checkout holds a resolution.
#[allow(clippy::too_many_arguments)]
async fn resolve_conflict(
    provider: &dyn Provider,
    model: &str,
    fixer: Option<&WorkerSpawner>,
    project_root: &std::path::Path,
    run_id: &str,
    integration_branch: &str,
    task_id: &str,
    files: &[String],
) -> bool {
    // J8 counts conflicts off the log, resolved or not.
    if let Ok(mut store) = RunStore::load(crate::run_dir(project_root, run_id)).await {
        let _ = store
            .append(Event::RunConflict {
                t: RunStore::now(),
                id: task_id.to_string(),
                files: files.to_vec(),
            })
            .await;
    }
    if resolve_conflicts_inline(provider, model, project_root, files).await {
        return true;
    }
    match fixer {
        Some(spawner) => {
            run_merge_fixer(
                spawner,
                project_root,
                run_id,
                integration_branch,
                task_id,
                files,
            )
            .await
        }
        None => false,
    }
}

/// E4 — spawn merge-fixer workers on a conflict, at most
/// [`MERGE_FIXER_ATTEMPTS`] of them. Each gets a fresh worktree at the
/// integration tip with `task_id`'s branch squash-merged in (so it holds the
/// conflict), the merge-fixer task carrying `task_id`'s acceptance checks, and
/// the summaries of the attempts before it; a retry escalates to the manager
/// model, as rung 2 of the retry ladder does. The first worker that ends in
/// Review with no conflict markers left has its tree carried into the
/// integration checkout and its task marked Done. `false` when none does,
/// which leaves the conflict to the blocked path.
async fn run_merge_fixer(
    spawner: &WorkerSpawner,
    project_root: &std::path::Path,
    run_id: &str,
    integration_branch: &str,
    task_id: &str,
    files: &[String],
) -> bool {
    let Ok(mut store) = RunStore::load(crate::run_dir(project_root, run_id)).await else {
        return false;
    };
    let fixer_id = merge_fixer_id(task_id);
    if store.state().task(&fixer_id).is_none() {
        record_merge_fixer_task(&mut store, task_id, files).await;
    }
    let Some(task) = store.state().task(&fixer_id).cloned() else {
        return false;
    };
    let store = Arc::new(tokio::sync::Mutex::new(store));
    let worktree = crate::worktree_dir(project_root, run_id, &fixer_id);
    let mut history: Vec<String> = Vec::new();
    for attempt in 0..MERGE_FIXER_ATTEMPTS {
        if let Err(e) = worktree::prepare_merge_fixer_worktree(
            project_root,
            integration_branch,
            run_id,
            task_id,
            &fixer_id,
            &worktree,
        ) {
            tracing::warn!(target: "pilot::pipeline", task = %task_id, error = %e, "could not set up the merge-fixer worktree");
            return false;
        }
        let agent_id = format!("{fixer_id}-{}", attempt + 1);
        let _ = store
            .lock()
            .await
            .append(Event::TaskAssign {
                t: RunStore::now(),
                id: fixer_id.clone(),
                agent: agent_id.clone(),
                worktree: worktree.display().to_string(),
            })
            .await;
        let ctx = orchestrator::SpawnContext {
            task: task.clone(),
            session_id: format!("pilot-{run_id}-{agent_id}"),
            agent_id,
            worktree: worktree.clone(),
            store: store.clone(),
            rung: attempt,
            escalate_model: attempt > 0,
            failure_history: history.clone(),
            cmd_rx: Arc::new(tokio::sync::Mutex::new(None)),
        };
        let (summary, outcome) = match spawner(ctx).await {
            Ok(r) if r.status == TaskStatus::Review => {
                match worktree::adopt_merge_fixer_resolution(project_root, &worktree, files) {
                    Ok(true) => {
                        let _ = store
                            .lock()
                            .await
                            .append(Event::TaskStatus {
                                t: RunStore::now(),
                                id: fixer_id.clone(),
                                status: TaskStatus::Done,
                                outcome: r.outcome,
                            })
                            .await;
                        return true;
                    }
                    Ok(false) => (
                        "reported the conflict resolved, but conflict markers remain".to_string(),
                        r.outcome,
                    ),
                    Err(e) => (
                        format!("its resolution could not be applied: {e}"),
                        r.outcome,
                    ),
                }
            }
            Ok(r) => (
                r.outcome
                    .as_ref()
                    .map(|o| o.summary.clone())
                    .unwrap_or_else(|| format!("ended {:?}", r.status)),
                r.outcome,
            ),
            Err(e) => (format!("worker spawn failed: {e}"), None),
        };
        tracing::warn!(
            target: "pilot::pipeline",
            task = %task_id, attempt = attempt + 1, "merge-fixer did not resolve the conflict: {summary}"
        );
        // The worker recorded its own status; this one says why the attempt
        // is not being used, which a worker that reported Review cannot know.
        let _ = store
            .lock()
            .await
            .append(Event::TaskStatus {
                t: RunStore::now(),
                id: fixer_id.clone(),
                status: TaskStatus::Failed,
                outcome: Some(crate::model::TaskOutcome {
                    summary: summary.clone(),
                    files_changed: outcome.map(|o| o.files_changed).unwrap_or_default(),
                }),
            })
            .await;
        history.push(format!("attempt {}: {summary}", attempt + 1));
    }
    false
}

async fn review_task_inline(
    provider: &dyn Provider,
    model: &str,
    task: &crate::model::Task,
    diff: &str,
    block_gate: crate::severity::Severity,
) -> (Option<String>, Option<InlineReview>) {
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
    // A failed call or unparseable reply means "no verdict", which the caller
    // records as *unreviewed* rather than silently counting as approved.
    let Ok((text, _usage)) = complete_text(provider, model, SYSTEM, &user).await else {
        return (None, None);
    };
    let Ok(report) = crate::review::parse_review(extract_json(&text)) else {
        return (None, None);
    };
    let rework = report.next_status(block_gate) == TaskStatus::Todo;
    let outcome = InlineReview {
        verdict: if rework {
            crate::review::Verdict::Rework
        } else {
            crate::review::Verdict::Approve
        },
        max_severity: report.max_severity(),
    };
    if rework {
        (Some(report.rework_notes(block_gate)), Some(outcome))
    } else {
        (None, Some(outcome))
    }
}

/// What the inline reviewer concluded about one task.
///
/// Recorded per task so the auto-merge gate can answer "was this diff actually
/// reviewed, and how bad was the worst finding" — previously the gate was
/// handed an empty verdict list and a `None` severity, so its
/// `auto_merge_max_severity` branch could never fire and `reviewed` reported
/// whether the reviewer was *enabled* rather than whether it *ran*.
#[derive(Debug, Clone, Copy)]
struct InlineReview {
    verdict: crate::review::Verdict,
    max_severity: Option<crate::severity::Severity>,
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
    let events = read_run_events(run_dir).await;
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
    events: &[Event],
    worker_model: &str,
) {
    for task in &state.tasks {
        let rec = crate::learning::StatRecord {
            run_id: state.run_id.clone(),
            role: task.role.as_str().to_string(),
            model: worker_model.to_string(),
            task_kind: None,
            first_try_ok: crate::learning::first_try_ok(events, task),
            pr_outcome: None, // R2 poller backfills this later
            goal: state.goal.clone(),
            t: RunStore::now(),
        };
        if let Err(e) = crate::learning::append_stat(stats_path, &rec) {
            tracing::warn!(target: "pilot::pipeline", error = %e, "failed to append stat record");
        }
    }
}

/// Id of the merge-fixer task for a conflict on `conflicting_task_id`.
fn merge_fixer_id(conflicting_task_id: &str) -> String {
    format!("merge-fixer-{conflicting_task_id}")
}

/// E4 — record a merge-fixer task on a merge conflict. Appends a
/// `task.create` for a [`crate::model::Role::MergeFixer`] whose `writes` are
/// the conflicted files, whose goal points at the conflicting task, and whose
/// acceptance checks are that task's, so the conflict is durable, resumable
/// work rather than a lost hard error. Best-effort: a failed append is logged,
/// not surfaced (the run is already stopped on the conflict).
async fn record_merge_fixer_task(
    store: &mut RunStore,
    conflicting_task_id: &str,
    files: &[String],
) {
    let acceptance = store
        .state()
        .task(conflicting_task_id)
        .map(|t| t.acceptance.clone())
        .unwrap_or_default();
    let ev = crate::Event::TaskCreate {
        t: RunStore::now(),
        id: merge_fixer_id(conflicting_task_id),
        role: crate::model::Role::MergeFixer,
        title: format!("Resolve merge conflict from task {conflicting_task_id}"),
        goal: format!(
            "Task {conflicting_task_id} conflicts with earlier integration work in: {}. \
             This worktree is the integration branch with {conflicting_task_id}'s changes \
             squash-merged in, conflict markers included. Resolve the conflict preserving \
             both sides' intent, re-run acceptance, and commit.",
            files.join(", ")
        ),
        // Ordered after the task it fixes, so a resumed run's merge reaches
        // the conflicting task first.
        deps: vec![conflicting_task_id.to_string()],
        writes: files.to_vec(),
        acceptance,
        reversibility: Default::default(),
        reversibility_reason: None,
    };
    if let Err(e) = store.append(ev).await {
        tracing::warn!(target: "pilot::pipeline", error = %e, "failed to record merge-fixer task");
    }
}

/// J8 — maintain the durable knowledge layer under `.wingman/knowledge/`
/// after a run merges:
///
/// - `hotspots.json` gains this run's edits and conflicts, read off `events`;
/// - `architecture.md` is re-rendered from the crates' `pub mod`s, under the
///   summary the knowledge-keeper agent writes when `keeper` is set (the
///   previous summary is kept when it is not, or its reply is unusable);
/// - `decisions.jsonl` gets the keeper's decisions, or without a usable keeper
///   reply one record for the run (its goal, and the task summaries as the
///   rationale).
///
/// Best-effort; every failure is logged and swallowed so a knowledge write can
/// never fail the run.
async fn maintain_knowledge(
    project_root: &std::path::Path,
    state: &crate::model::RunState,
    events: &[Event],
    keeper: Option<&AuxAgent>,
    usage: &mut wingman_core::Usage,
) {
    use crate::knowledge;
    let dir = knowledge::knowledge_dir(project_root);

    let hotspots_path = knowledge::hotspots_path(&dir);
    let mut hotspots = knowledge::load_hotspots(&hotspots_path);
    hotspots.observe_run(events);
    if let Err(e) = knowledge::save_hotspots(&hotspots_path, &hotspots) {
        tracing::warn!(target: "pilot::pipeline", error = %e, "failed to write hotspots.json");
    }

    // Module map: each `crates/<name>/src/lib.rs` → its `pub mod`s.
    let crates = discover_crate_modules(&project_root.join("crates"));
    let arch_path = knowledge::architecture_path(&dir);
    let previous = std::fs::read_to_string(&arch_path)
        .ok()
        .and_then(|md| knowledge::architecture_summary(&md));
    let report = match keeper {
        Some(k) => run_knowledge_keeper(k, state, &crates, previous.as_deref(), usage).await,
        None => None,
    };
    let summary = report
        .as_ref()
        .map(|r| r.summary.as_str())
        .or(previous.as_deref());
    if let Err(e) = std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::write(&arch_path, knowledge::render_architecture(summary, &crates)))
    {
        tracing::warn!(target: "pilot::pipeline", error = %e, "failed to write architecture.md");
    }

    let records: Vec<knowledge::DecisionRecord> = match report {
        Some(r) => r
            .decisions
            .into_iter()
            .take(KEEPER_MAX_DECISIONS)
            .map(|d| knowledge::DecisionRecord {
                run_id: state.run_id.clone(),
                t: RunStore::now(),
                decision: d.decision,
                rationale: d.rationale,
            })
            .collect(),
        None => vec![knowledge::DecisionRecord {
            run_id: state.run_id.clone(),
            t: RunStore::now(),
            decision: state.goal.clone(),
            rationale: state
                .tasks
                .iter()
                .filter_map(|t| t.outcome.as_ref().map(|o| o.summary.clone()))
                .collect::<Vec<_>>()
                .join("; "),
        }],
    };
    for rec in &records {
        if let Err(e) = knowledge::append_decision(&knowledge::decisions_path(&dir), rec) {
            tracing::warn!(target: "pilot::pipeline", error = %e, "failed to append decision record");
        }
    }
}

/// Most decisions one knowledge-keeper reply may append.
const KEEPER_MAX_DECISIONS: usize = 3;

/// J8 — the knowledge-keeper agent: given the run, the module map and the
/// current architecture summary, it returns the revised summary and the
/// architectural decisions the run made. `None` on a failed call or a reply
/// that does not parse.
async fn run_knowledge_keeper(
    keeper: &AuxAgent,
    state: &crate::model::RunState,
    crates: &[(String, Vec<String>)],
    previous_summary: Option<&str>,
    usage: &mut wingman_core::Usage,
) -> Option<crate::knowledge::KeeperReport> {
    const SYSTEM: &str = "You maintain a project's architecture notes after an autonomous \
        run merges. Reply with ONLY a JSON object: {\"summary\":\"...\",\"decisions\":\
        [{\"decision\":\"...\",\"rationale\":\"...\"}]}. `summary` is the whole updated \
        architecture summary in Markdown, at most about 300 words: how the project is \
        organised and why, revised for what this run changed. Describe the code, not the \
        run. `decisions` lists only architectural choices this run made that later work \
        should respect, at most 3; leave it empty when the run made none.";
    let tasks: Vec<String> = state
        .tasks
        .iter()
        .map(|t| {
            let (summary, files) = t
                .outcome
                .as_ref()
                .map(|o| (o.summary.as_str(), o.files_changed.join(", ")))
                .unwrap_or(("(no summary)", String::new()));
            format!(
                "- #{} [{}] {} — {summary} (files: {files})",
                t.id,
                t.role.as_str(),
                t.title
            )
        })
        .collect();
    let modules: Vec<String> = crates
        .iter()
        .map(|(name, mods)| format!("- {name}: {}", mods.join(", ")))
        .collect();
    let user = format!(
        "Goal: {}\n\nTasks:\n{}\n\nCrates and their public modules:\n{}\n\n\
         Current architecture summary:\n{}",
        state.goal,
        tasks.join("\n"),
        modules.join("\n"),
        previous_summary.unwrap_or("(none yet)"),
    );
    match complete_text(keeper.provider.as_ref(), &keeper.model, SYSTEM, &user).await {
        Ok((text, u)) => {
            usage.add(&u);
            let report = crate::knowledge::parse_keeper_report(extract_json(&text));
            if report.is_none() {
                tracing::warn!(target: "pilot::pipeline", "knowledge-keeper reply did not parse; keeping the previous summary");
            }
            report
        }
        Err(e) => {
            tracing::warn!(target: "pilot::pipeline", error = %e, "knowledge-keeper call failed");
            None
        }
    }
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
                    .map(|rest| {
                        rest.trim_end_matches(';')
                            .split_whitespace()
                            .next()
                            .unwrap_or("")
                    })
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

/// The run's event log, or nothing when it cannot be read. For the best-effort
/// post-run passes that read the log; none of them may fail the run.
async fn read_run_events(run_dir: &std::path::Path) -> Vec<Event> {
    match RunStore::load(run_dir).await {
        Ok(store) => store.read_events().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// J15 — add each trigger in `more` that `found` does not already hold as the
/// same incident.
fn merge_escalations(
    found: &mut Vec<crate::escalation::EscalationTrigger>,
    more: &[crate::escalation::EscalationTrigger],
) {
    for t in more {
        if !found.iter().any(|f| f.duplicates(t)) {
            found.push(t.clone());
        }
    }
}

/// R3 — render + write the escalation packet for a blocked run. Returns
/// the packet path on success, or `None` if writing failed (best-effort;
/// a failed packet write must not mask the run's real failure). `events` is
/// the run log, read for the blocked task's retry-ladder attempts.
fn write_escalation_packet(
    project_root: &std::path::Path,
    run_id: &str,
    state: &crate::model::RunState,
    tier: wingman_config::PilotTier,
    triggers: &[crate::escalation::EscalationTrigger],
    events: &[Event],
) -> Option<PathBuf> {
    let blocked_task = state
        .tasks
        .iter()
        .find(|t| matches!(t.status, TaskStatus::Failed | TaskStatus::Blocked));
    let attempts = blocked_task
        .map(|t| crate::handoff::attempts_for(events, &t.id))
        .unwrap_or_default();
    let packet = crate::handoff::HandoffPacket {
        state,
        tier,
        blocked_task,
        triggers,
        attempts: &attempts,
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
pub(crate) mod tests {
    use super::*;
    use crate::model::{Event, Role, RunStatus, Task, TaskStatus};
    use crate::orchestrator::{fake_happy_spawner, OrchestratorConfig};
    use crate::pr::{CommandOut, CommandRunner};

    #[test]
    fn changed_lockfiles_finds_each_lockfile_once() {
        let mut d = DiffLines::default();
        d.changed.push(("src/main.rs".into(), "x".into()));
        d.changed.push(("crates/foo/Cargo.toml".into(), "y".into()));
        assert!(changed_lockfiles(&d).is_empty());
        d.changed.push(("Cargo.lock".into(), "+a".into()));
        d.changed.push(("Cargo.lock".into(), "-b".into()));
        d.changed
            .push(("panel/package-lock.json".into(), "+c".into()));
        assert_eq!(
            changed_lockfiles(&d),
            vec![
                "Cargo.lock".to_string(),
                "panel/package-lock.json".to_string()
            ]
        );
        assert_eq!(
            lockfile_dir(Path::new("/repo"), "panel/package-lock.json"),
            Path::new("/repo").join("panel")
        );
    }

    #[test]
    fn collect_dependency_licenses_parses_cargo_metadata() {
        struct MetaRunner;
        impl CommandRunner for MetaRunner {
            fn run(
                &self,
                program: &str,
                args: &[&str],
                _cwd: &std::path::Path,
            ) -> std::io::Result<CommandOut> {
                assert_eq!(program, "cargo");
                // Never rewrites the user's lockfile.
                assert!(args.contains(&"--locked"), "{args:?}");
                Ok(CommandOut {
                    status: Some(0),
                    stdout: r#"{"packages":[
                        {"name":"foo","version":"1.0.0","license":"MIT"},
                        {"name":"bar","version":"0.2.0","license":null}
                    ]}"#
                    .into(),
                    stderr: String::new(),
                })
            }
        }
        let deps = collect_dependency_licenses(&MetaRunner, std::path::Path::new("."))
            .expect("metadata parsed");
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0], ("foo".into(), "1.0.0".into(), "MIT".into()));
        assert_eq!(deps[1], ("bar".into(), "0.2.0".into(), String::new()));
        // And the scan flags the empty license as a finding.
        let pairs: Vec<(String, String)> = deps.into_iter().map(|(n, _, l)| (n, l)).collect();
        let findings = crate::security::scan_licenses(&pairs, "Cargo.lock", &["MIT".into()], &[]);
        assert_eq!(findings.len(), 1);
        // A failing `cargo metadata` is `None`, not an empty (clean) list.
        assert!(collect_dependency_licenses(&AllFailRunner, Path::new(".")).is_none());
    }

    /// Scripted runner for the R6 external-tool paths. `tools_present`
    /// decides whether gitleaks / cargo-audit exist; when they do, gitleaks
    /// writes a report to `--report-path` and cargo audit prints one advisory.
    struct SecurityToolsRunner {
        tools_present: bool,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }
    impl SecurityToolsRunner {
        fn new(tools_present: bool) -> Self {
            Self {
                tools_present,
                calls: Mutex::new(Vec::new()),
            }
        }
    }
    impl CommandRunner for SecurityToolsRunner {
        fn run(&self, program: &str, args: &[&str], _cwd: &Path) -> std::io::Result<CommandOut> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
            let ok = |stdout: &str| {
                Ok(CommandOut {
                    status: Some(0),
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                })
            };
            const OLD_LOCK: &str =
                "[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\nsource = \"registry+x\"\n";
            const NEW_LOCK: &str = "[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\nsource = \"registry+x\"\n\n[[package]]\nname = \"copyleft\"\nversion = \"0.1.0\"\nsource = \"registry+x\"\n\n[[package]]\nname = \"me\"\nversion = \"0.1.0\"\n";
            match (program, args.first().copied().unwrap_or("")) {
                ("git", "diff") => ok(concat!(
                    "--- a/Cargo.lock\n+++ b/Cargo.lock\n+name = \"copyleft\"\n",
                    "--- a/web/package-lock.json\n+++ b/web/package-lock.json\n+x\n",
                )),
                ("git", "show") => match args[1] {
                    "base123:Cargo.lock" => ok(OLD_LOCK),
                    "wingman/auto/r1:Cargo.lock" => ok(NEW_LOCK),
                    "base123:web/package-lock.json" => ok(r#"{"packages":{}}"#),
                    "wingman/auto/r1:web/package-lock.json" => ok(
                        r#"{"packages":{"node_modules/agpl-thing":{"version":"1.0.0","license":"AGPL-3.0"}}}"#,
                    ),
                    _ => Ok(CommandOut {
                        status: Some(128),
                        stdout: String::new(),
                        stderr: "fatal: path does not exist".into(),
                    }),
                },
                ("cargo", "metadata") => ok(r#"{"packages":[
                    {"name":"serde","version":"1.0.0","license":"GPL-3.0"},
                    {"name":"copyleft","version":"0.1.0","license":"GPL-3.0"},
                    {"name":"me","version":"0.1.0","license":"GPL-3.0"}
                ]}"#),
                ("gitleaks", _) | ("cargo", "audit") if !self.tools_present => {
                    if program == "gitleaks" {
                        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "not found"))
                    } else {
                        Ok(CommandOut {
                            status: Some(101),
                            stdout: String::new(),
                            stderr: "error: no such command: `audit`".into(),
                        })
                    }
                }
                ("gitleaks", "version") => ok("8.21.2"),
                ("gitleaks", "detect") => {
                    let at = args.iter().position(|a| *a == "--report-path").unwrap();
                    std::fs::write(
                        args[at + 1],
                        r#"[{"Description":"AWS","RuleID":"aws-access-token","File":"a.rs","StartLine":3}]"#,
                    )?;
                    ok("")
                }
                ("cargo", "audit") => Ok(CommandOut {
                    // cargo-audit exits 1 when it finds something.
                    status: Some(1),
                    stdout: r#"{"vulnerabilities":{"list":[{"advisory":{"id":"RUSTSEC-2099-0001","title":"bad"},"package":{"name":"copyleft"}}]}}"#.into(),
                    stderr: String::new(),
                }),
                _ => ok(""),
            }
        }
    }

    /// R6 — the license scan reads the lockfiles the run changed, checks only
    /// the packages it added (not `serde`, already at the base, nor the
    /// workspace's own crate), and applies the deny list to npm too.
    #[test]
    fn r6_license_scan_checks_only_packages_the_run_added() {
        let cfg = wingman_config::PilotSecurityConfig {
            secrets_scanner: String::new(),
            dependency_audit: false,
            denied_licenses: vec!["AGPL-3.0".into()],
            ..Default::default()
        };
        let runner = SecurityToolsRunner::new(true);
        let report = run_security_pass(&runner, Path::new("."), "base123", "wingman/auto/r1", &cfg);
        let licenses: Vec<&crate::security::SecurityFinding> = report
            .findings
            .iter()
            .filter(|f| f.kind == "license")
            .collect();
        assert_eq!(licenses.len(), 2, "{:?}", report.findings);
        assert!(licenses.iter().any(|f| f.message.contains("`copyleft`")
            && f.severity == crate::severity::Severity::High
            && f.file.as_deref() == Some("Cargo.lock")));
        assert!(licenses.iter().any(|f| f.message.contains("`agpl-thing`")
            && f.severity == crate::severity::Severity::Critical
            && f.file.as_deref() == Some("web/package-lock.json")));
        assert!(report
            .notes
            .iter()
            .any(|n| n.contains("1 new package(s) in `Cargo.lock`")));
    }

    /// R6 — gitleaks and cargo audit run when installed, and their findings
    /// land in the report next to the built-in scan's.
    #[test]
    fn r6_external_scanners_run_when_present() {
        let runner = SecurityToolsRunner::new(true);
        let report = run_security_pass(
            &runner,
            Path::new("."),
            "base123",
            "wingman/auto/r1",
            &wingman_config::PilotSecurityConfig::default(),
        );
        assert!(report
            .findings
            .iter()
            .any(|f| f.message.contains("aws-access-token")));
        assert!(report
            .findings
            .iter()
            .any(|f| f.message.contains("RUSTSEC-2099-0001")));
        assert!(report
            .notes
            .iter()
            .any(|n| n.starts_with("gitleaks: 1 finding(s)")));
        assert!(report
            .notes
            .iter()
            .any(|n| n.starts_with("cargo audit: 1 advisory")));
        // gitleaks scanned exactly the run's commits, with redaction on.
        let calls = runner.calls.lock().unwrap();
        let detect = calls
            .iter()
            .find(|(p, a)| p == "gitleaks" && a.first().map(String::as_str) == Some("detect"))
            .expect("gitleaks detect ran");
        assert!(detect.1.contains(&"base123..wingman/auto/r1".to_string()));
        assert!(detect.1.contains(&"--redact".to_string()));
    }

    /// R6 — a host without gitleaks or cargo-audit still gets a security pass,
    /// and the summary says which scanners did not run instead of implying a
    /// clean bill of health.
    #[test]
    fn r6_missing_external_scanners_degrade_and_say_so() {
        let runner = SecurityToolsRunner::new(false);
        let report = run_security_pass(
            &runner,
            Path::new("."),
            "base123",
            "wingman/auto/r1",
            &wingman_config::PilotSecurityConfig::default(),
        );
        assert!(report
            .notes
            .iter()
            .any(|n| n.contains("`gitleaks` not found on PATH")));
        assert!(report
            .notes
            .iter()
            .any(|n| n.contains("cargo-audit is not installed")));
        let md = crate::security::render_report(&report, crate::severity::Severity::Medium);
        assert!(md.contains("not found on PATH"), "{md}");

        // A scanner nobody taught us the CLI of is noted, never invoked.
        let cfg = wingman_config::PilotSecurityConfig {
            secrets_scanner: "trufflehog".into(),
            ..Default::default()
        };
        let runner = SecurityToolsRunner::new(true);
        let report = run_security_pass(&runner, Path::new("."), "base123", "wingman/auto/r1", &cfg);
        assert!(report
            .notes
            .iter()
            .any(|n| n.contains("`trufflehog` is not supported")));
        assert!(!runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(p, _)| p == "trufflehog"));
    }

    #[test]
    fn r6_security_summary_is_posted_as_a_pr_comment() {
        let runner = RecordingRunner::new();
        post_security_comment(
            &runner,
            Path::new("."),
            "https://github.com/test/repo/pull/9",
            "# Security pass\n\n✅ No findings.\n",
        );
        let long = "x".repeat(MAX_PR_COMMENT_BYTES + 10);
        post_security_comment(
            &runner,
            Path::new("."),
            "https://github.com/test/repo/pull/9",
            &long,
        );
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "gh");
        assert_eq!(
            calls[0].1[..3],
            ["pr", "comment", "https://github.com/test/repo/pull/9"]
        );
        assert!(calls[0].1[4].contains("No findings"));
        assert!(calls[1].1[4].len() <= MAX_PR_COMMENT_BYTES + 32);
        assert!(calls[1].1[4].ends_with("(truncated)\n"));
    }
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::Mutex;
    use tempfile::tempdir;
    use wingman_core::{
        AgentEvent, AgentStop, CompletionRequest, ContentBlock, Message, Provider,
        ProviderCapabilities, ProviderEventStream, Role as ApiRole, StopReason, StreamEvent, Usage,
    };

    /// Scripted provider: each `complete()` call peeks at the LAST user
    /// message in the request to figure out where the run is and returns
    /// the appropriate tool-use block. The manager system prompt + the
    /// rendered state block are part of the input so the provider can
    /// branch on what the manager is seeing.
    pub(crate) struct ScriptedProvider {
        call_count: Mutex<u32>,
    }

    impl ScriptedProvider {
        pub(crate) fn new() -> Self {
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
                reasoning: false,
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

            // One scheduling step per tick, like a real manager: once the
            // step's tool result is back, end the turn. The state text in the
            // history is still the pre-step picture, so acting on it again
            // repeats a step the orchestrator has already taken.
            let answered = req.messages.last().is_some_and(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            });
            if answered {
                let events = vec![Ok(StreamEvent::Stop {
                    reason: StopReason::EndTurn,
                })];
                return Ok(Box::pin(stream::iter(events)));
            }

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
    pub(crate) struct AllOkCommandRunner;
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

    /// [`AllOkCommandRunner`] that also records every call into a handle the
    /// test keeps, for pipelines that take ownership of their runner.
    type CallLog = Arc<Mutex<Vec<(String, Vec<String>)>>>;
    struct SharedRecorder(CallLog);
    impl CommandRunner for SharedRecorder {
        fn run(&self, program: &str, args: &[&str], cwd: &Path) -> std::io::Result<CommandOut> {
            self.0.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
            AllOkCommandRunner.run(program, args, cwd)
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
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let inputs = PipelineInputs {
            provider: Arc::new(ScriptedProvider::new()),
            manager_model: "stub".into(),
            worker_spawner: fake_happy_spawner(),
            base_branch: "main".into(),
            project_root: project_root.clone(),
            command_runner: Box::new(SharedRecorder(recorded.clone())),
            no_pr: false,
            orchestrator_cfg: OrchestratorConfig {
                max_concurrent_agents: 4,
                task_timeout: std::time::Duration::from_secs(30),
                project_root: project_root.clone(),
                run_id: run_id.into(),
                base_commit: base_commit.clone(),
                use_real_worktrees: true,
                max_usd: 0.0,
                max_total_tokens: 0,
                max_retries_per_task: 0,
                desktop_inbox: None,
                sample_host_load: false,
                speculative_prespawn: false,
                warm_cmd: String::new(),
            },
            max_ticks: 32,
            tier: wingman_config::PilotTier::Copilot,
            worker_model: "stub-worker".into(),
            stats_path: None,
            auto_approved: false,
            pr_config: wingman_config::PilotPrConfig::default(),
            security_config: wingman_config::PilotSecurityConfig::default(),
            disabled_tools: Vec::new(),
            run_reviewer: false,
            critic: None,
            reviewer_model: "stub".into(),
            sandbox_default_tier: "host".into(),
            sandbox_availability: crate::sandbox::TierAvailability {
                docker: false,
                vm: Err("test".into()),
            },
            dangerous_paths: Vec::new(),
            merge_fixer: true,
            knowledge_keeper: None,
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
        // R6: the security pass ran and its summary was posted on that PR.
        assert!(outcome.security.is_some());
        assert!(
            recorded.lock().unwrap().iter().any(|(p, a)| p == "gh"
                && a.len() == 5
                && a[..3] == ["pr", "comment", pr.url.as_str()]
                && a[4].starts_with("# Security pass")),
            "no security comment was posted"
        );

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
            &[Event::TaskAttempt {
                t: RunStore::now(),
                id: "t1".into(),
                agent: "agent-0003".into(),
                rung: 2,
                model: Some("big-model".into()),
                status: TaskStatus::Failed,
                summary: "acceptance checks failed: 0/1 green".into(),
                tests: Default::default(),
            }],
        )
        .expect("packet written");

        assert!(path.ends_with("escalation.md"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# Escalation: blocked-run"));
        assert!(body.contains("blocked at task #t1"));
        assert!(body.contains("the hard part"));
        assert!(body.contains("wingman pilot resume blocked-run"));
        // The ladder's recorded attempts fill "What was tried".
        assert!(body.contains("rung 2, model `big-model`"), "{body}");
        assert!(body.contains("0/1 green"));
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
            &[],
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

    /// First-try is read from the attempts, not the final status: t1 passed on
    /// its only rung-0 attempt, t2 reached Done only after a retry, and t3 was
    /// split after failing (the splitter marks the parent Done).
    #[test]
    fn e6_record_run_stats_writes_one_per_task() {
        let dir = tempdir().unwrap();
        let stats = dir.path().join("stats.jsonl");
        let mut state = crate::model::RunState::new("r1", "the goal", "abc", "b");
        let mut tasks = Vec::new();
        for (id, role) in [
            ("t1", Role::Developer),
            ("t2", Role::Tester),
            ("t3", Role::Developer),
        ] {
            let mut t = Task::new(id, role, "x");
            t.status = TaskStatus::Done;
            tasks.push(t);
        }
        state.tasks = tasks;
        let attempt = |id: &str, rung: u32, status: TaskStatus| Event::TaskAttempt {
            t: String::new(),
            id: id.into(),
            agent: format!("{id}-{rung}"),
            rung,
            model: None,
            status,
            summary: String::new(),
            tests: Default::default(),
        };
        let events = vec![
            attempt("t1", 0, TaskStatus::Review),
            attempt("t2", 0, TaskStatus::Failed),
            attempt("t2", 1, TaskStatus::Review),
            attempt("t3", 0, TaskStatus::Failed),
        ];

        record_run_stats(&stats, &state, &events, "haiku");

        let loaded = crate::learning::load_stats(&stats).unwrap();
        assert_eq!(loaded.len(), 3);
        assert!(loaded.iter().all(|r| r.model == "haiku"));
        let first_try: Vec<bool> = loaded.iter().map(|r| r.first_try_ok).collect();
        assert_eq!(first_try, vec![true, false, false]);
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
                reasoning: false,
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
        // No Docker, no vm backend → vm/container degrade to host.
        let tiers = compute_sandbox_tiers(&state, "host", &avail(false, false));
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers.iter().find(|(id, _)| id == "t1").unwrap().1, "host");
        // t2 selects vm but degrades to host without a backend.
        assert_eq!(tiers.iter().find(|(id, _)| id == "t2").unwrap().1, "host");
    }

    fn avail(docker: bool, vm: bool) -> crate::sandbox::TierAvailability {
        crate::sandbox::TierAvailability {
            docker,
            vm: if vm { Ok(()) } else { Err("no kvm".into()) },
        }
    }

    #[test]
    fn j11_keeps_vm_tier_only_with_a_vm_backend() {
        let mut state = crate::model::RunState::new("r1", "g", "abc", "b");
        state.tasks.push({
            let mut t = Task::new("t2", Role::Developer, "migrate");
            t.writes = vec!["db/migrations/001.sql".into()];
            t
        });
        assert_eq!(
            compute_sandbox_tiers(&state, "host", &avail(false, true))[0].1,
            "vm"
        );
        // Docker is a container, not a vm: the task reports what it got.
        assert_eq!(
            compute_sandbox_tiers(&state, "host", &avail(true, false))[0].1,
            "container"
        );
    }

    #[test]
    fn j11_sandbox_default_is_a_floor() {
        let mut state = crate::model::RunState::new("r1", "g", "abc", "b");
        state.tasks.push(Task::new("t1", Role::Developer, "edit"));
        // Default container floor lifts even a plain task to container —
        // when Docker is available.
        let tiers = compute_sandbox_tiers(&state, "container", &avail(true, false));
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
        let (notes, outcome) = notes;
        assert!(notes.is_some(), "rework verdict must return notes");
        assert!(notes.unwrap().contains("no error handling"));
        // The verdict is now recorded too, so the auto-merge gate can see it.
        let outcome = outcome.expect("a parsed review must yield a verdict");
        assert_eq!(outcome.verdict, crate::review::Verdict::Rework);
        assert!(outcome.max_severity.is_some());
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
        let (notes, outcome) = notes;
        assert!(notes.is_none(), "garbage must fail open to approve");
        // But an unparseable reply is *not* a review: recording it as one
        // would let the auto-merge gate believe a diff was reviewed when the
        // reviewer never actually produced a verdict.
        assert!(
            outcome.is_none(),
            "an unparseable reply must not count as a recorded review"
        );
    }

    /// A git repo whose tasks `t1` and `t2` (both in Review) each add
    /// `shared.txt` with different content on their own branch, so squashing
    /// `t2` into the integration branch conflicts. `.wingman/` is ignored, as
    /// in a real project. `None` when git is unavailable.
    fn conflicting_repo(
        run_id: &str,
    ) -> Option<(tempfile::TempDir, PathBuf, crate::model::RunState)> {
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
            return None;
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
        std::fs::write(repo.join(".gitignore"), ".wingman/\n").unwrap();
        gitc(&repo, &["add", "-A"]).unwrap();
        gitc(&repo, &["commit", "-qm", "seed"]).unwrap();
        let base = String::from_utf8(gitc(&repo, &["rev-parse", "HEAD"]).unwrap().stdout)
            .unwrap()
            .trim()
            .to_string();

        let mut state =
            crate::model::RunState::new(run_id, "g", &base, crate::integration_branch(run_id));
        for (id, body) in [("t1", "A"), ("t2", "B")] {
            let mut task = Task::new(id, Role::Developer, format!("edit {id}"));
            task.status = TaskStatus::Review;
            state.tasks.push(task);
            let wt = crate::worktree_dir(&repo, run_id, id);
            crate::worktree::create_worktree(&repo, &base, run_id, id, &wt).unwrap();
            std::fs::write(wt.join("shared.txt"), format!("base\n{body}\n")).unwrap();
            gitc(&wt, &["add", "-A"]).unwrap();
            gitc(&wt, &["commit", "-qm", "edit"]).unwrap();
        }
        Some((tmp, repo, state))
    }

    /// `<repo>:<branch>:<path>` via `git show`.
    fn show(repo: &Path, branch: &str, path: &str) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["show", &format!("{branch}:{path}")])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolver_bridge_resolves_conflict_via_block_in_place() {
        // Exercises the exact live path: the sync merge calls a resolver that
        // bridges to the async `resolve_conflicts_inline` via block_in_place +
        // block_on. Uses a canned provider so it's deterministic and offline.
        let Some((_tmp, repo, state)) = conflicting_repo("bridge") else {
            eprintln!("skipping: git not available");
            return;
        };

        // Canned model returns a clean merged file.
        let provider = CannedTextProvider {
            text: "base\nA\nB\n".into(),
        };
        let repo_c = repo.clone();
        let resolver = move |_task_id: &str, files: &[String]| -> bool {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(resolve_conflicts_inline(&provider, "m", &repo_c, files))
            })
        };

        let outcome = crate::worktree::merge_integration_with_resolver(
            &repo,
            &state.base_commit,
            &state.integration_branch,
            &state,
            Some(&resolver),
        )
        .expect("resolver bridge should land the conflicting task");
        assert_eq!(outcome.commits.len(), 2);
        let merged = std::fs::read_to_string(repo.join("shared.txt")).unwrap();
        assert!(merged.contains('A') && merged.contains('B') && !merged.contains("<<<<<<<"));
    }

    /// The run store for [`conflicting_repo`]'s plan; `t2` carries an
    /// acceptance check the merge-fixer must inherit.
    async fn conflicting_run_store(repo: &Path, state: &crate::model::RunState) {
        let mut store = RunStore::create(
            crate::run_dir(repo, &state.run_id),
            &state.run_id,
            "g",
            &state.base_commit,
            &state.integration_branch,
        )
        .await
        .unwrap();
        for task in &state.tasks {
            store
                .append(Event::TaskCreate {
                    t: RunStore::now(),
                    id: task.id.clone(),
                    role: Role::Developer,
                    title: task.title.clone(),
                    goal: String::new(),
                    deps: Vec::new(),
                    writes: vec!["shared.txt".into()],
                    acceptance: if task.id == "t2" {
                        vec![crate::model::Acceptance::Grep {
                            pattern: "B".into(),
                            path: "shared.txt".into(),
                        }]
                    } else {
                        Vec::new()
                    },
                    reversibility: Default::default(),
                    reversibility_reason: None,
                })
                .await
                .unwrap();
        }
    }

    /// Merge `state` with the pipeline's resolver: a one-shot model that
    /// always hands the markers back, then merge-fixers from `spawner`.
    fn merge_with_fixer(
        repo: &Path,
        state: &crate::model::RunState,
        spawner: WorkerSpawner,
    ) -> Result<IntegrationMergeOutcome, crate::worktree::WorktreeError> {
        let provider = CannedTextProvider {
            text: "<<<<<<< still\nA\n=======\nB\n>>>>>>> broken".into(),
        };
        let resolver = |task_id: &str, files: &[String]| -> bool {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(resolve_conflict(
                    &provider,
                    "m",
                    Some(&spawner),
                    repo,
                    &state.run_id,
                    &state.integration_branch,
                    task_id,
                    files,
                ))
            })
        };
        crate::worktree::merge_integration_with_resolver(
            repo,
            &state.base_commit,
            &state.integration_branch,
            state,
            Some(&resolver),
        )
    }

    fn spawn_result(
        ctx: &crate::orchestrator::SpawnContext,
        status: TaskStatus,
        summary: &str,
    ) -> crate::orchestrator::WorkerSpawnResult {
        crate::orchestrator::WorkerSpawnResult {
            agent_id: ctx.agent_id.clone(),
            status,
            outcome: Some(crate::model::TaskOutcome {
                summary: summary.into(),
                files_changed: vec!["shared.txt".into()],
            }),
        }
    }

    /// E4: when the one-shot resolver fails, a merge-fixer worker runs in a
    /// worktree holding the conflict. An attempt that claims success with the
    /// markers still in place is refused; the retry (on the escalated model,
    /// told what went wrong) resolves it, and the conflicting task lands.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn e4_merge_fixer_resolves_what_the_one_shot_resolver_cannot() {
        let Some((_tmp, repo, state)) = conflicting_repo("fixer") else {
            eprintln!("skipping: git not available");
            return;
        };
        conflicting_run_store(&repo, &state).await;

        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_c = calls.clone();
        let spawner: WorkerSpawner = Arc::new(move |ctx: crate::orchestrator::SpawnContext| {
            let n = calls_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                let held = std::fs::read_to_string(ctx.worktree.join("shared.txt")).unwrap();
                assert!(held.contains("<<<<<<<"), "the fixer starts on the conflict");
                assert_eq!(ctx.task.role, Role::MergeFixer);
                assert_eq!(ctx.task.acceptance.len(), 1, "t2's checks are inherited");
                if n == 0 {
                    assert!(!ctx.escalate_model && ctx.failure_history.is_empty());
                    return Ok(spawn_result(&ctx, TaskStatus::Review, "done, honest"));
                }
                assert!(ctx.escalate_model);
                assert!(ctx.failure_history[0].contains("conflict markers remain"));
                std::fs::write(ctx.worktree.join("shared.txt"), "base\nA\nB\n").unwrap();
                Ok(spawn_result(&ctx, TaskStatus::Review, "merged both lines"))
            })
        });

        let outcome = merge_with_fixer(&repo, &state, spawner).expect("the fixer lands the task");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(outcome.commits.len(), 2);
        assert_eq!(
            show(&repo, &state.integration_branch, "shared.txt"),
            "base\nA\nB\n"
        );

        let store = RunStore::load(crate::run_dir(&repo, &state.run_id))
            .await
            .unwrap();
        let fixer = store.state().task("merge-fixer-t2").expect("fixer task");
        assert_eq!(fixer.status, TaskStatus::Done);
        assert_eq!(fixer.attempts, 2);
        assert_eq!(fixer.deps, vec!["t2".to_string()]);
        let events = store.read_events().await.unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            Event::RunConflict { id, files, .. } if id == "t2" && files == &["shared.txt".to_string()]
        )));
        let _ = crate::worktree::cleanup_worktrees(&repo, &state.run_id);
    }

    /// E4: merge-fixer attempts are bounded; when every one fails the
    /// conflict surfaces as before, with the fixer task Failed and saying why.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn e4_merge_fixer_gives_up_after_its_attempts() {
        let Some((_tmp, repo, state)) = conflicting_repo("fixer-fail") else {
            eprintln!("skipping: git not available");
            return;
        };
        conflicting_run_store(&repo, &state).await;

        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_c = calls.clone();
        let spawner: WorkerSpawner = Arc::new(move |ctx: crate::orchestrator::SpawnContext| {
            calls_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { Ok(spawn_result(&ctx, TaskStatus::Failed, "acceptance red")) })
        });

        match merge_with_fixer(&repo, &state, spawner) {
            Err(crate::worktree::WorktreeError::Conflict { task_id, .. }) => {
                assert_eq!(task_id, "t2")
            }
            other => panic!("expected the conflict to surface, got {other:?}"),
        }
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            MERGE_FIXER_ATTEMPTS
        );
        let store = RunStore::load(crate::run_dir(&repo, &state.run_id))
            .await
            .unwrap();
        let fixer = store.state().task("merge-fixer-t2").expect("fixer task");
        assert_eq!(fixer.status, TaskStatus::Failed);
        assert_eq!(
            fixer.outcome.as_ref().map(|o| o.summary.as_str()),
            Some("acceptance red")
        );
        let _ = crate::worktree::cleanup_worktrees(&repo, &state.run_id);
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
        assert!(
            !run_critic_pass(&provider, "m", &state, &mut wingman_core::Usage::default()).await
        );
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
            reasoning: Default::default(),
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
        assert_eq!(
            t.writes,
            vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
        );
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
        let md = crate::knowledge::render_architecture(None, &got);
        assert!(md.contains("wingman-foo") && md.contains("alpha"));
    }

    /// J8: the knowledge-keeper's summary and decisions land in the knowledge
    /// layer alongside the module map and this run's hotspots; a later run
    /// whose keeper reply is unusable keeps that summary and falls back to one
    /// plain decision record, and the hotspots keep accumulating.
    #[tokio::test]
    async fn j8_maintain_knowledge_runs_the_keeper_and_falls_back() {
        use crate::knowledge;
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let src = root.join("crates").join("wingman-foo").join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("lib.rs"), "pub mod alpha;\n").unwrap();
        let mut state = crate::model::RunState::new("r1", "split the parser", "abc", "b");
        state.tasks = vec![done_task("t1")];
        let events = vec![
            Event::TaskStatus {
                t: RunStore::now(),
                id: "t1".into(),
                status: TaskStatus::Review,
                outcome: Some(crate::model::TaskOutcome {
                    summary: "s".into(),
                    files_changed: vec!["src/parse.rs".into()],
                }),
            },
            Event::RunConflict {
                t: RunStore::now(),
                id: "t1".into(),
                files: vec!["src/parse.rs".into()],
            },
        ];
        let keeper = |text: &str| AuxAgent {
            provider: Arc::new(CannedTextProvider { text: text.into() }),
            model: "fast".into(),
        };
        let dir = knowledge::knowledge_dir(root);

        let mut usage = wingman_core::Usage::default();
        maintain_knowledge(
            root,
            &state,
            &events,
            Some(&keeper(
                r#"{"summary":"Parsing lives in `alpha`.","decisions":[{"decision":"one parser per format","rationale":"formats diverge"}]}"#,
            )),
            &mut usage,
        )
        .await;
        let md = std::fs::read_to_string(knowledge::architecture_path(&dir)).unwrap();
        assert_eq!(
            knowledge::architecture_summary(&md).as_deref(),
            Some("Parsing lives in `alpha`.")
        );
        assert!(md.contains("- `alpha`"));
        let decisions = knowledge::load_decisions(&knowledge::decisions_path(&dir)).unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].decision, "one parser per format");

        maintain_knowledge(root, &state, &events, Some(&keeper("no json")), &mut usage).await;
        let md = std::fs::read_to_string(knowledge::architecture_path(&dir)).unwrap();
        assert_eq!(
            knowledge::architecture_summary(&md).as_deref(),
            Some("Parsing lives in `alpha`.")
        );
        let decisions = knowledge::load_decisions(&knowledge::decisions_path(&dir)).unwrap();
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[1].decision, "split the parser");
        let hot = knowledge::load_hotspots(&knowledge::hotspots_path(&dir));
        assert_eq!(
            (
                hot.edit_count("src/parse.rs"),
                hot.conflict_count("src/parse.rs")
            ),
            (2, 2)
        );
        let ctx = knowledge::render_planner_context(root).unwrap();
        assert!(ctx.contains("Parsing lives in") && ctx.contains("src/parse.rs"));
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
