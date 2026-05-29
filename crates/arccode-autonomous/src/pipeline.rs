//! End-to-end pilot pipeline: plan → workers → merge → PR.
//!
//! Glues together every other module so the CLI doesn't have to know
//! about the moving parts:
//!
//! 1. Spawn the orchestrator actor with [`orchestrator::spawn`].
//! 2. Build the manager [`arccode_core::AgentLoop`] via
//!    [`manager::build_manager`].
//! 3. Run [`manager::drive_to_completion`] until every task is terminal.
//! 4. If every task ended in `Done`, run
//!    [`worktree::merge_integration`] then
//!    [`pr::open_pull_request`].
//! 5. Cleanup worker worktrees.
//!
//! Used by both `arccode pilot run` (fresh run after planning) and
//! `arccode pilot resume` (existing run loaded from disk).

use std::path::PathBuf;
use std::sync::Arc;

use arccode_core::Provider;
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
}

/// Outcome of one full pipeline run.
#[derive(Debug, Clone)]
pub struct PipelineOutcome {
    pub merged: Option<IntegrationMergeOutcome>,
    pub pr: Option<PrOutcome>,
    pub failed_tasks: Vec<String>,
}

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

    let (handle, join) = orchestrator::spawn(store, inputs.orchestrator_cfg, inputs.worker_spawner);

    // Drive the manager loop. Manager system prompt is loaded inside
    // build_manager; the per-tick state block is injected by
    // drive_to_completion.
    let cwd = std::env::current_dir().unwrap_or_else(|_| project_root.clone());
    let registry = build_manager_registry(handle.clone(), cwd, project_root.clone());
    let mut agent = build_manager(inputs.provider, inputs.manager_model, registry, None);

    drive_to_completion(&mut agent, &handle, inputs.max_ticks).await?;

    // Manager exited. Grab the final state and decide whether to merge.
    let final_state = handle.snapshot().await?;
    handle.shutdown().await;
    let _ = join.await;

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
        return Ok(PipelineOutcome {
            merged: None,
            pr: None,
            failed_tasks: failed,
        });
    }

    // The manager may finalize tasks incrementally (calling finalize_task
    // per-task as workers report Review). When that happens, there are no
    // Review-status tasks left at run-end and merge_integration is a no-op.
    // When the manager skips finalize, the pipeline does it here at the
    // end. Either way, we still want to open a PR.
    let need_merge = final_state
        .tasks
        .iter()
        .any(|t| t.status == TaskStatus::Review);

    let run_dir = crate::run_dir(&project_root, &run_id);
    let mut store = RunStore::load(&run_dir).await?;

    let merge_outcome = if need_merge {
        let outcome = worktree::merge_integration(
            &project_root,
            &final_state.base_commit,
            &integration_branch,
            &final_state,
        )?;
        pr::finalize_all_review_tasks(&mut store, &final_state, &outcome.commits).await?;
        Some(outcome)
    } else {
        // Manager finalized tasks incrementally. The integration branch
        // may not exist yet (orchestrator.handle_finalize emits a
        // RunMergeTask event but doesn't run git). Run the merge now
        // against an empty task list — merge_integration will only
        // process Review tasks, so this is essentially a no-op other
        // than creating the integration branch ref pointing at base.
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
    // tidy if the PR step errors out.
    let _removed = worktree::cleanup_worktrees(&project_root, &run_id);

    if inputs.no_pr {
        store
            .append(crate::Event::RunDone { t: RunStore::now() })
            .await?;
        return Ok(PipelineOutcome {
            merged: merge_outcome,
            pr: None,
            failed_tasks: Vec::new(),
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

    Ok(PipelineOutcome {
        merged: merge_outcome,
        pr: Some(pr_outcome),
        failed_tasks: Vec::new(),
    })
}

/// Mark tasks stuck in `InProgress` as `Failed` so the retry watchdog
/// picks them up on resume. Used at the start of `arccode pilot resume`.
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
    use arccode_core::{
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
                cache_kind: arccode_core::CacheKind::None,
            }
        }
        async fn complete(
            &self,
            req: CompletionRequest,
        ) -> arccode_core::Result<ProviderEventStream> {
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
            },
            max_ticks: 32,
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

    // Silence unused-import warnings if the test gates above ever skip.
    #[allow(dead_code)]
    fn _unused_imports() {
        let _: Option<Task> = None;
        let _: AgentEvent = AgentEvent::Stop {
            reason: AgentStop::EndTurn,
        };
    }
}
