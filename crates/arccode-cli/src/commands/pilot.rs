//! `arccode pilot <GOAL>` — entry point for pilot mode.
//!
//! Phase 2 ships planning end-to-end: pick provider, resolve git base,
//! create the run directory, call the planner, render and approve, persist
//! `task.create` events. The orchestrator that spawns workers and merges
//! into a PR lands in Phases 3–6.

use std::io::Write;
use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use arccode_autonomous::{
    integration_branch,
    planner::{parse_plan, persist_plan, plan_from_goal, render_plan, PlannerLlm, ProviderLlm},
    run_dir, RunStore,
};
use arccode_config::{Config, ProjectPaths};

use crate::runtime;

/// Options forwarded from the clap subcommand.
pub struct PilotOptions {
    pub goal: String,
    pub tier: Option<String>,
    pub plan_only: bool,
    pub yes: bool,
    pub review: bool,
    #[allow(dead_code)] // reserved for in-terminal tail of an in-process run (Phase 7.8 / E12)
    pub watch: bool,
    pub no_pr: bool,
    pub base: Option<String>,
    pub max_agents: Option<u32>,
    pub max_usd: Option<f64>,
    pub sandbox: Option<String>,
    pub channel: Option<String>,
    pub model_override: Option<String>,
}

pub async fn run(cfg: Config, opts: PilotOptions) -> Result<ExitCode> {
    // Resolve the effective pilot config: tier override is the only flag the
    // user can flip without editing config. Other overrides (max_agents,
    // max_usd) get applied to a clone so the rest of the run sees them.
    let mut pilot = cfg.pilot.clone();
    if let Some(t) = opts.tier.as_deref() {
        pilot.tier = t.parse().map_err(|e: String| anyhow!(e))?;
    }
    if let Some(n) = opts.max_agents {
        pilot.max_concurrent_agents = n;
    }
    if let Some(u) = opts.max_usd {
        pilot.max_usd = u;
    }
    if let Some(s) = opts.sandbox.as_deref() {
        pilot.sandbox.default_tier = s.to_string();
    }
    if let Some(c) = opts.channel.as_deref() {
        pilot.approval.notify_channel = c.to_string();
    }

    // Planner model resolution: prefer pilot.default_model, then --model,
    // then the global default. The same Provider trait the TUI uses.
    let planner_model = pilot
        .default_model
        .clone()
        .or_else(|| opts.model_override.clone())
        .or_else(|| cfg.default_model.clone());
    let selection = runtime::resolve_selection(&cfg, planner_model.as_deref())?;
    if let Err(why) = arccode_autonomous::provider_support::gate_run(&selection.provider_id) {
        return Err(anyhow!(why));
    }
    eprintln!(
        "{}",
        arccode_autonomous::provider_support::support_notice(&selection.provider_id)
    );
    let provider = runtime::build_provider(&cfg, &selection.provider_id)
        .with_context(|| format!("building provider for {}", selection.provider_id))?;

    // Pin the run to the current git HEAD (or the user's --base override).
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let base_commit = resolve_base_commit(&project.root, opts.base.as_deref())?;
    let run_id = new_run_id();
    let integration = integration_branch(&run_id);
    let run_path = run_dir(&project.root, &run_id);

    eprintln!(
        "[pilot] run {run_id} · tier={} · planner={}/{} · base={}",
        pilot.tier, selection.provider_id, selection.model, &base_commit[..8.min(base_commit.len())]
    );
    eprintln!("[pilot] planning…");

    let mut store = RunStore::create(&run_path, &run_id, &opts.goal, &base_commit, &integration)
        .await
        .context("opening run store")?;

    // The planner is a one-shot completion. The provider lives behind an
    // Arc<dyn Provider>; ProviderLlm borrows it via the trait.
    let llm = ProviderLlm {
        provider: provider.as_ref(),
        model: selection.model.clone(),
        max_tokens: 4096,
    };
    let plan = plan_from_goal(&llm as &dyn PlannerLlm, &opts.goal, &project.root)
        .await
        .context("planner call failed")?;

    eprintln!("[pilot] proposed {} task(s) (run id: {run_id}).", plan.len());
    eprint!("\n{}", render_plan(&plan));

    // E1 trust-tiered approval. Classifier decides whether to proceed
    // silently (auto), surface a veto window (notify-only), or fall
    // back to the y/e/n prompt (hard).
    let report = arccode_autonomous::approval::classify(
        arccode_autonomous::approval::ClassifyInputs {
            plan: &plan,
            config: &pilot.approval,
            tier: pilot.tier,
            force_auto: opts.yes,
            force_hard: opts.review,
        },
    );
    eprintln!(
        "[pilot] approval: {} (est. ${:.2}) — {}",
        report.tier, report.estimated_usd, report.reason
    );

    let approve = match report.tier {
        arccode_autonomous::approval::ApprovalTier::Auto => true,
        arccode_autonomous::approval::ApprovalTier::NotifyOnly => {
            run_notify_window(
                &plan,
                &opts.goal,
                pilot.approval.notify_only_window_secs,
                &pilot.approval.notify_channel,
            )
            .await?
        }
        arccode_autonomous::approval::ApprovalTier::Hard => {
            if !std::io::stdin().is_terminal() {
                eprintln!(
                    "[pilot] hard-gate required and no TTY — refusing to auto-approve plan."
                );
                false
            } else {
                prompt_for_approval(&plan, &opts.goal)?
            }
        }
    };

    if !approve {
        eprintln!("[pilot] plan rejected; not persisting tasks.");
        return Ok(ExitCode::from(2));
    }

    persist_plan(&mut store, &plan)
        .await
        .context("persisting plan to tasks.jsonl")?;

    eprintln!(
        "[pilot] wrote plan ({n} tasks) to {path}",
        n = plan.len(),
        path = store.log_path().display(),
    );

    if opts.plan_only {
        eprintln!("[pilot] --plan-only: stopping before worker spawn.");
        return Ok(ExitCode::SUCCESS);
    }

    let base_branch =
        std::env::var("ARCCODE_PILOT_BASE_BRANCH").unwrap_or_else(|_| "main".into());
    let orch_cfg = arccode_autonomous::orchestrator::OrchestratorConfig {
        max_concurrent_agents: pilot.max_concurrent_agents,
        task_timeout: std::time::Duration::from_secs(pilot.task_timeout_secs),
        project_root: project.root.clone(),
        run_id: run_id.clone(),
        base_commit: base_commit.clone(),
        use_real_worktrees: true,
        max_usd: pilot.max_usd,
        max_retries_per_task: 1,
    };
    let inputs = arccode_autonomous::pipeline::PipelineInputs {
        provider,
        manager_model: selection.model.clone(),
        worker_spawner: build_real_worker_spawner(
            pilot.worker_model.as_deref().unwrap_or(&selection.model),
        )?,
        base_branch,
        project_root: project.root.clone(),
        command_runner: Box::new(arccode_autonomous::pr::SystemCommandRunner),
        no_pr: opts.no_pr,
        orchestrator_cfg: orch_cfg,
        max_ticks: 64,
    };

    eprintln!("[pilot] driving manager loop ({} ticks max)…", inputs.max_ticks);
    let outcome = arccode_autonomous::pipeline::run_to_completion(store, inputs)
        .await
        .context("pipeline run_to_completion")?;
    if !outcome.failed_tasks.is_empty() {
        eprintln!(
            "[pilot] some tasks did not reach Done: {:?}",
            outcome.failed_tasks
        );
        return Ok(ExitCode::from(2));
    }
    if let Some(pr) = outcome.pr {
        eprintln!("[pilot] PR opened: {}", pr.url);
    } else if outcome.merged.is_some() {
        eprintln!("[pilot] integration branch ready; PR step skipped (--no-pr).");
    }
    Ok(ExitCode::SUCCESS)
}

/// Resolve the base commit for the run. `--base <REV>` overrides; otherwise
/// we pin to current HEAD.
fn resolve_base_commit(repo_root: &std::path::Path, base: Option<&str>) -> Result<String> {
    let rev = base.unwrap_or("HEAD");
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("rev-parse")
        .arg(rev)
        .output()
        .with_context(|| format!("running `git rev-parse {rev}`"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git rev-parse {rev} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Generate a run id of the form `YYYY-MM-DD-HHMM-<rand6>`.
fn new_run_id() -> String {
    use rand::Rng;
    let now = chrono::Utc::now();
    let suffix: String = rand::thread_rng()
        .sample_iter(rand::distributions::Alphanumeric)
        .take(6)
        .map(|c| (c as char).to_ascii_lowercase())
        .collect();
    format!("{}-{suffix}", now.format("%Y-%m-%d-%H%M"))
}

/// Interactive `y / e / n` prompt. `e` opens $EDITOR on the plan JSON, then
/// reparses the edited file. Returns true when the (possibly edited) plan
/// should proceed.
fn prompt_for_approval(
    plan: &[arccode_autonomous::planner::PlannedTask],
    _goal: &str,
) -> Result<bool> {
    // We keep the plan immutable from the caller's perspective for now —
    // editing rewrites a fresh JSON file but the persisted plan still uses
    // the model-emitted one until edit-in-place lands in Phase 7.6 (E2).
    loop {
        eprint!("Approve plan? [y / e (edit) / n] ");
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading stdin")?;
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" | "" => return Ok(false),
            "e" | "edit" => {
                if let Err(e) = open_plan_in_editor(plan) {
                    eprintln!("[pilot] editor failed: {e}");
                }
                // Edit-in-place is a Phase 7.6 enhancement; for now we
                // re-prompt with the original plan so the user can still
                // approve or cancel.
                continue;
            }
            other => {
                eprintln!("[pilot] unrecognised input '{other}' — answer y, e, or n.");
            }
        }
    }
}

/// Write the plan JSON to a temp file and open $EDITOR on it. Caller can
/// inspect or hand-edit; the edited file is not yet re-ingested (Phase 7.6
/// E2 wires that loop).
fn open_plan_in_editor(plan: &[arccode_autonomous::planner::PlannedTask]) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| {
            if cfg!(target_os = "windows") {
                "notepad".into()
            } else {
                "vi".into()
            }
        });
    let tmp = std::env::temp_dir().join(format!("arccode-plan-{}.json", std::process::id()));
    let body = serde_json::to_string_pretty(&serde_json::json!({ "tasks": plan }))
        .context("serializing plan for editor")?;
    std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    let status = std::process::Command::new(&editor)
        .arg(&tmp)
        .status()
        .with_context(|| format!("launching {editor}"))?;
    if !status.success() {
        anyhow::bail!("editor exited with status {status}");
    }
    // Best-effort: try to re-parse so the user knows whether their edits
    // were syntactically valid, but discard the result for now.
    let edited = std::fs::read_to_string(&tmp).ok();
    if let Some(body) = edited {
        match parse_plan(&body) {
            Ok(p) => eprintln!(
                "[pilot] edited plan parses cleanly ({} tasks); approval flow will re-use the original until E2 edit-in-place lands.",
                p.len()
            ),
            Err(e) => eprintln!("[pilot] edited plan failed to parse: {e}"),
        }
    }
    std::fs::remove_file(&tmp).ok();
    Ok(())
}

/// Trait shim — `std::io::Stdin::is_terminal` is stable since 1.70 but
/// brought in via the `IsTerminal` trait.
use std::io::IsTerminal;

/// Notify-only veto window. Prints the plan summary + the configured
/// notify channel hint, then sleeps for `window_secs`. If the user
/// presses Enter (or sends Ctrl+C) the run is vetoed; otherwise we
/// proceed. Non-interactive sessions auto-proceed silently — the
/// classifier already decided this plan is safe enough to run unattended.
async fn run_notify_window(
    plan: &[arccode_autonomous::planner::PlannedTask],
    goal: &str,
    window_secs: u64,
    channel: &str,
) -> Result<bool> {
    let _ = goal;
    let count = plan.len();
    eprintln!(
        "[pilot] notify-only: {count} tasks in plan, vetoing window {window_secs}s (channel: {channel})."
    );
    eprintln!("[pilot] press Enter within the window to veto; ignore to proceed.");
    if !std::io::stdin().is_terminal() {
        // Non-interactive: just wait the window out so an operator
        // watching logs has a chance to interrupt with SIGTERM.
        tokio::time::sleep(std::time::Duration::from_secs(window_secs)).await;
        eprintln!("[pilot] notify window elapsed; proceeding.");
        return Ok(true);
    }

    // Read a single line from stdin on a blocking task; race it against
    // the timeout. Tokio's tokio::io::stdin requires the `io-std`
    // feature which the workspace doesn't enable; spawn_blocking is the
    // workspace-friendly alternative.
    let read_line = tokio::task::spawn_blocking(|| {
        let mut buf = String::new();
        let _ = std::io::stdin().read_line(&mut buf);
        buf
    });
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(window_secs));
    tokio::pin!(timeout);
    tokio::pin!(read_line);
    tokio::select! {
        _ = &mut read_line => {
            eprintln!("[pilot] veto received; rejecting plan.");
            Ok(false)
        }
        _ = &mut timeout => {
            eprintln!("[pilot] notify window elapsed; proceeding.");
            Ok(true)
        }
    }
}

// ----------------------------------------------------------------------
// `arccode pilot resume`
// ----------------------------------------------------------------------

/// Resume an interrupted run. Loads the existing RunStore, marks stuck
/// InProgress tasks as Failed so the retry watchdog picks them up, then
/// re-enters the same end-to-end pipeline that `pilot run` uses.
pub async fn resume(
    cfg: Config,
    run_id: String,
    no_pr: bool,
    model_override: Option<String>,
) -> Result<ExitCode> {
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let run_path = arccode_autonomous::run_dir(&project.root, &run_id);
    if !run_path.exists() {
        return Err(anyhow!(
            "no run directory at {} — run id {run_id} not found",
            run_path.display()
        ));
    }

    let mut store = arccode_autonomous::RunStore::load(&run_path)
        .await
        .with_context(|| format!("loading run {run_id}"))?;

    let stuck = arccode_autonomous::pipeline::mark_stale_in_progress_failed(&mut store)
        .await
        .context("marking stale tasks")?;
    if !stuck.is_empty() {
        eprintln!(
            "[pilot] resume: marked {} stuck task(s) as Failed: {:?}",
            stuck.len(),
            stuck
        );
    }

    // Resolve the same manager provider as a fresh run would.
    let planner_model = cfg
        .pilot
        .default_model
        .clone()
        .or(model_override)
        .or_else(|| cfg.default_model.clone());
    let selection = runtime::resolve_selection(&cfg, planner_model.as_deref())?;
    if let Err(why) = arccode_autonomous::provider_support::gate_run(&selection.provider_id) {
        return Err(anyhow!(why));
    }
    let provider = runtime::build_provider(&cfg, &selection.provider_id)
        .with_context(|| format!("building provider {}", selection.provider_id))?;

    let state = store.state().clone();
    let base_branch = std::env::var("ARCCODE_PILOT_BASE_BRANCH").unwrap_or_else(|_| "main".into());
    let orch_cfg = arccode_autonomous::orchestrator::OrchestratorConfig {
        max_concurrent_agents: cfg.pilot.max_concurrent_agents,
        task_timeout: std::time::Duration::from_secs(cfg.pilot.task_timeout_secs),
        project_root: project.root.clone(),
        run_id: run_id.clone(),
        base_commit: state.base_commit.clone(),
        use_real_worktrees: true,
        max_usd: cfg.pilot.max_usd,
        max_retries_per_task: 1,
    };
    let inputs = arccode_autonomous::pipeline::PipelineInputs {
        provider,
        manager_model: selection.model.clone(),
        worker_spawner: build_real_worker_spawner(&selection.model)?,
        base_branch,
        project_root: project.root,
        command_runner: Box::new(arccode_autonomous::pr::SystemCommandRunner),
        no_pr,
        orchestrator_cfg: orch_cfg,
        max_ticks: 64,
    };

    eprintln!("[pilot] resume: driving manager loop for run {run_id}");
    let outcome = arccode_autonomous::pipeline::run_to_completion(store, inputs)
        .await
        .context("pipeline run_to_completion")?;
    if !outcome.failed_tasks.is_empty() {
        eprintln!(
            "[pilot] resume: tasks ended in non-Done state: {:?}",
            outcome.failed_tasks
        );
        return Ok(ExitCode::from(2));
    }
    if let Some(pr) = outcome.pr {
        eprintln!("[pilot] resume: PR URL → {}", pr.url);
    }
    Ok(ExitCode::SUCCESS)
}

/// Build the production WorkerSpawner: spawns real `arccode --worker-mode`
/// child processes via [`arccode_autonomous::worker::run_worker`].
fn build_real_worker_spawner(_worker_model: &str) -> Result<arccode_autonomous::orchestrator::WorkerSpawner> {
    let arccode_bin = std::env::current_exe().context("locating arccode binary")?;
    Ok(std::sync::Arc::new(move |ctx: arccode_autonomous::orchestrator::SpawnContext| {
        let arccode_bin = arccode_bin.clone();
        Box::pin(async move {
            // Translate SpawnContext to WorkerSpec and drive run_worker.
            // The orchestrator's store handle is exposed through ctx; we
            // build a minimal WorkerSpec here.
            //
            // NOTE: run_worker takes its OWN &mut RunStore reference, not
            // an Arc<Mutex<RunStore>>. To keep this self-contained without
            // refactoring the worker module, we acquire a lock for the
            // duration of the worker's lifetime — which is fine because
            // the manager actor processes one assign at a time.
            let spec = arccode_autonomous::worker::WorkerSpec {
                arccode_bin,
                task: ctx.task.clone(),
                role: ctx.task.role.clone(),
                worktree: ctx.worktree.clone(),
                session_id: ctx.session_id.clone(),
                model: None, // worker reads pilot.worker_model from config
                timeout: std::time::Duration::from_secs(1800),
            };
            let mut store_guard = ctx.store.lock().await;
            let result = arccode_autonomous::worker::run_worker(
                &mut *store_guard,
                &ctx.agent_id,
                spec,
            )
            .await
            .map_err(|e| {
                arccode_autonomous::orchestrator::OrchestratorError::Spawn(e.to_string())
            })?;
            Ok(arccode_autonomous::orchestrator::WorkerSpawnResult {
                agent_id: ctx.agent_id,
                status: result.status,
                outcome: result.outcome,
            })
        })
    }))
}

// ----------------------------------------------------------------------
// `arccode pilot status` and `arccode pilot watch`
// ----------------------------------------------------------------------

/// One-shot dashboard print. Picks the most recently updated run unless
/// the user names one. Exits non-zero if no runs exist under
/// `<project>/.arccode/autonomous/`.
pub async fn status(run_id: Option<String>) -> Result<ExitCode> {
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let runs = arccode_autonomous::dashboard::list_runs(&project.root)
        .context("listing runs")?;
    if runs.is_empty() {
        eprintln!("[pilot] no runs found under {}", project.root.display());
        return Ok(ExitCode::from(1));
    }
    let pick = match run_id {
        Some(id) => runs
            .iter()
            .find(|r| r.run_id == id)
            .cloned()
            .ok_or_else(|| anyhow!("no run with id {id} found"))?,
        None => runs.into_iter().next().unwrap(),
    };
    let state = arccode_autonomous::dashboard::load_state(&pick.dir)?;
    let recent = arccode_autonomous::dashboard::tail_events(&pick.dir, 12)?;
    let view = arccode_autonomous::dashboard::render_dashboard(&state, &recent);
    print!("{}", view.to_ascii());
    Ok(ExitCode::SUCCESS)
}

/// Live-watch a run. Polls `<run-dir>/state.json` mtime every
/// `interval_ms` and redraws the dashboard whenever it advances. Ctrl-C
/// to exit.
///
/// We deliberately keep this lightweight (no full crossterm raw-mode
/// initialization) so it composes with normal scrollback the way `tail
/// -f` does. The dashboard re-renders by reprinting the box on each
/// tick.
pub async fn watch(run_id: Option<String>, interval_ms: u64) -> Result<ExitCode> {
    use std::time::Duration;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let runs = arccode_autonomous::dashboard::list_runs(&project.root)?;
    if runs.is_empty() {
        eprintln!("[pilot] no runs found under {}", project.root.display());
        return Ok(ExitCode::from(1));
    }
    let pick = match run_id {
        Some(id) => runs
            .iter()
            .find(|r| r.run_id == id)
            .cloned()
            .ok_or_else(|| anyhow!("no run with id {id} found"))?,
        None => runs.into_iter().next().unwrap(),
    };

    eprintln!(
        "[pilot] watching {} (Ctrl-C to exit)",
        pick.dir.display()
    );

    let interval = Duration::from_millis(interval_ms.max(50));
    let mut last_mtime = None;
    loop {
        let mtime = arccode_autonomous::dashboard::state_mtime(&pick.dir);
        if mtime != last_mtime {
            last_mtime = mtime;
            match (
                arccode_autonomous::dashboard::load_state(&pick.dir),
                arccode_autonomous::dashboard::tail_events(&pick.dir, 12),
            ) {
                (Ok(state), Ok(recent)) => {
                    // Clear screen between frames with the ANSI sequence;
                    // plain enough to work on Windows console + cmd, gnome-
                    // terminal, kitty, iTerm without dragging in crossterm
                    // raw-mode plumbing.
                    print!("\x1b[2J\x1b[H");
                    let view =
                        arccode_autonomous::dashboard::render_dashboard(&state, &recent);
                    print!("{}", view.to_ascii());
                    if matches!(
                        state.status,
                        arccode_autonomous::RunStatus::Done
                            | arccode_autonomous::RunStatus::Failed
                            | arccode_autonomous::RunStatus::Aborted
                    ) {
                        eprintln!("[pilot] run reached terminal state — exiting watch loop.");
                        return Ok(ExitCode::SUCCESS);
                    }
                }
                (Err(e), _) | (_, Err(e)) => {
                    eprintln!("[pilot] failed to read run state: {e}");
                }
            }
        }
        tokio::time::sleep(interval).await;
    }
}
