//! `wingman pilot <GOAL>` — entry point for pilot mode.
//!
//! Phase 2 ships planning end-to-end: pick provider, resolve git base,
//! create the run directory, call the planner, render and approve, persist
//! `task.create` events. The orchestrator that spawns workers and merges
//! into a PR lands in Phases 3–6.

use std::io::Write;
use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use wingman_autonomous::{
    integration_branch,
    planner::{parse_plan, persist_plan, render_plan, PlannerLlm, ProviderLlm},
    run_dir, RunStore,
};
use wingman_config::{Config, ProjectPaths};

use crate::runtime;

/// Resolve whether the dashboard should render plain-ASCII glyphs instead of
/// the unicode status/spinner glyphs.
///
/// Precedence: an explicit `--ascii` flag wins; then the `WINGMAN_ASCII`
/// escape hatch (`0`/`false`/`no` forces unicode, anything else forces
/// ASCII); otherwise we auto-detect. The auto path is conservative — it only
/// downgrades to ASCII on terminals that historically can't render the
/// glyphs (legacy Windows console; a clearly non-UTF-8 unix locale).
pub fn resolve_ascii(flag: bool) -> bool {
    if flag {
        return true;
    }
    if let Some(v) = std::env::var_os("WINGMAN_ASCII") {
        let v = v.to_string_lossy();
        let off = matches!(v.trim(), "0" | "false" | "no" | "off" | "");
        return !off;
    }
    auto_ascii()
}

/// Best-effort guess at whether the current terminal can't render the unicode
/// glyphs. Kept dependency-free: we key off well-known environment markers
/// rather than probing the console API.
fn auto_ascii() -> bool {
    if cfg!(windows) {
        // Modern terminals (Windows Terminal, VS Code, ConEmu) render the
        // glyphs fine and advertise themselves; the legacy conhost/cmd host
        // does not, so default it to ASCII.
        let modern = std::env::var_os("WT_SESSION").is_some()
            || std::env::var_os("TERM_PROGRAM").is_some()
            || std::env::var_os("ConEmuANSI").is_some();
        !modern
    } else {
        // On unix, only downgrade when the locale is explicitly non-UTF-8.
        // An unset locale is treated as capable (the modern default).
        let loc = std::env::var("LC_ALL")
            .or_else(|_| std::env::var("LC_CTYPE"))
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_default();
        !loc.is_empty() && !loc.to_ascii_uppercase().contains("UTF")
    }
}

/// Resolve which run a control command targets: an explicit id, else the
/// most recently updated run under the project.
fn pick_run(run_id: Option<String>) -> Result<wingman_autonomous::dashboard::RunSummary> {
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let runs = wingman_autonomous::dashboard::list_runs(&project.root).context("listing runs")?;
    if runs.is_empty() {
        return Err(anyhow!("no runs found under {}", project.root.display()));
    }
    match run_id {
        Some(id) => runs
            .into_iter()
            .find(|r| r.run_id == id)
            .ok_or_else(|| anyhow!("no run with id {id} found")),
        None => Ok(runs.into_iter().next().unwrap()),
    }
}

/// Append a control command to the selected run's control channel.
fn send_control(
    run_id: Option<String>,
    cmd: wingman_autonomous::control::ControlCommand,
) -> Result<ExitCode> {
    let pick = pick_run(run_id)?;
    wingman_autonomous::control::append(&pick.dir, &cmd)
        .with_context(|| format!("writing control command to run {}", pick.run_id))?;
    eprintln!("[pilot] {} → {}", cmd.encode(), pick.run_id);
    Ok(ExitCode::SUCCESS)
}

/// `pilot abort [run] [--task T]` — abort the whole run, or just one task.
pub async fn control_abort(run_id: Option<String>, task: Option<String>) -> Result<ExitCode> {
    use wingman_autonomous::control::ControlCommand;
    let cmd = match task {
        Some(id) => ControlCommand::AbortTask { id },
        None => ControlCommand::AbortRun,
    };
    send_control(run_id, cmd)
}

/// `pilot retry <task> [run]` — re-queue a failed/blocked task.
pub async fn control_retry(run_id: Option<String>, task: String) -> Result<ExitCode> {
    send_control(
        run_id,
        wingman_autonomous::control::ControlCommand::RetryTask { id: task },
    )
}

/// `pilot approve [run]` — release a plan-approval gate.
pub async fn control_approve(run_id: Option<String>) -> Result<ExitCode> {
    send_control(run_id, wingman_autonomous::control::ControlCommand::Approve)
}

/// `pilot veto [run]` — reject a pending plan.
pub async fn control_veto(run_id: Option<String>) -> Result<ExitCode> {
    send_control(run_id, wingman_autonomous::control::ControlCommand::Veto)
}

/// E6 adaptive-routing thresholds: a role's cheap-model blended success
/// rate must clear this (after `ROUTE_MIN_SAMPLES` attempts) to keep being
/// routed to the cheap model; otherwise it escalates to the capable model.
const ROUTE_SUCCESS_THRESHOLD: f64 = 0.7;
const ROUTE_MIN_SAMPLES: u32 = 3;

/// Load the E6 cross-run stats and aggregate them for adaptive routing.
/// Returns `None` when there's no stats file or it's empty, so a fresh
/// install routes purely on the configured worker model.
fn load_routing_aggregates(
    stats_path: Option<&std::path::Path>,
) -> Option<std::sync::Arc<wingman_autonomous::learning::Aggregates>> {
    let path = stats_path?;
    let records = wingman_autonomous::learning::load_stats(path).ok()?;
    if records.is_empty() {
        return None;
    }
    Some(std::sync::Arc::new(
        wingman_autonomous::learning::aggregate(records),
    ))
}

/// Options forwarded from the clap subcommand.
#[derive(Default)]
pub struct PilotOptions {
    pub goal: String,
    pub tier: Option<String>,
    pub plan_only: bool,
    pub yes: bool,
    pub review: bool,
    /// E12 — tail the in-process run with a compact progress line.
    pub watch: bool,
    /// Run detached: re-exec self in the background, print the run id, and
    /// return the shell prompt. Watch/control via `pilot watch|abort <id>`.
    pub detached: bool,
    pub no_pr: bool,
    pub base: Option<String>,
    pub max_agents: Option<u32>,
    pub max_usd: Option<f64>,
    pub sandbox: Option<String>,
    pub channel: Option<String>,
    /// Wait for a control-channel approve/veto on a headless hard gate instead
    /// of refusing outright.
    pub await_approval: bool,
    /// Seconds to wait when `await_approval` is set before rejecting.
    pub approval_timeout_secs: u64,
    pub model_override: Option<String>,
}

pub async fn run(cfg: Config, opts: PilotOptions) -> Result<ExitCode> {
    // `-d`/`--detached`: if we're the top-level invocation (not the re-exec'd
    // child), mint the run id, spawn a detached copy of ourselves writing to
    // the run's log, print the id, and hand the shell back. The child re-enters
    // this function with WINGMAN_DETACHED_CHILD set and runs the pipeline.
    let detached_child = std::env::var_os("WINGMAN_DETACHED_CHILD").is_some();
    if opts.detached && !detached_child {
        let project = ProjectPaths::discover(&std::env::current_dir()?);
        let run_id = new_run_id();
        let run_path = run_dir(&project.root, &run_id);
        return spawn_detached(&run_id, &run_path).map(|()| ExitCode::SUCCESS);
    }

    // Tree-kill live workers on Ctrl+C / SIGTERM instead of orphaning them.
    // Covers both the plain foreground run and the detached child (where a
    // `kill <pid>` should still reap the worker trees).
    crate::shutdown::install();

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
    if let Err(why) = wingman_autonomous::provider_support::gate_run(&selection.provider_id) {
        return Err(anyhow!(why));
    }
    eprintln!(
        "{}",
        wingman_autonomous::provider_support::support_notice(&selection.provider_id)
    );
    let provider = runtime::build_provider(&cfg, &selection.provider_id)
        .with_context(|| format!("building provider for {}", selection.provider_id))?;

    // J1 — goal refinement & negotiation (autopilot, or wherever the
    // `goal_refinement` capability is enabled). Runs a refinement agent
    // before planning; it may restate an ambiguous goal, challenge it, or
    // ask clarifying questions. The (possibly restated) goal flows into the
    // rest of the run. `None` means the user vetoed → abort cleanly.
    let goal = if capability_on(&pilot, "goal_refinement") {
        match refine_goal(provider.as_ref(), &selection.model, &opts.goal, &pilot).await {
            Some(g) => g,
            None => {
                eprintln!("[pilot] goal refinement: aborted before planning.");
                return Ok(ExitCode::from(2));
            }
        }
    } else {
        opts.goal.clone()
    };

    // Pin the run to the current git HEAD (or the user's --base override).
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let base_commit = resolve_base_commit(&project.root, opts.base.as_deref())?;
    // A detached child inherits the run id its parent minted (so the id it
    // prints and the log path it writes to line up with the child's run).
    let run_id = std::env::var("WINGMAN_RUN_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(new_run_id);
    let integration = integration_branch(&run_id);
    let run_path = run_dir(&project.root, &run_id);

    eprintln!(
        "[pilot] run {run_id} · tier={} · planner={}/{} · base={}",
        pilot.tier,
        selection.provider_id,
        selection.model,
        &base_commit[..8.min(base_commit.len())]
    );
    eprintln!("[pilot] planning…");

    let mut store = RunStore::create(&run_path, &run_id, &goal, &base_commit, &integration)
        .await
        .context("opening run store")?;

    // The planner is a one-shot completion. The provider lives behind an
    // Arc<dyn Provider>; ProviderLlm borrows it via the trait.
    let llm = ProviderLlm {
        provider: provider.as_ref(),
        model: selection.model.clone(),
        max_tokens: 4096,
    };
    // E6 — prime the planner with the most similar past runs and their
    // outcomes (merged / reverted), so the draft pass can lean toward what
    // worked. Best-effort: no stats file → no priming.
    let priming = wingman_config::global_dir()
        .ok()
        .map(|g| g.join("stats.jsonl"))
        .and_then(|p| wingman_autonomous::learning::load_stats(&p).ok())
        .filter(|records| !records.is_empty())
        .and_then(|records| wingman_autonomous::learning::render_priming(&goal, &records, 5));
    if priming.is_some() {
        eprintln!("[pilot] priming planner with similar past runs (E6).");
    }
    let plan = wingman_autonomous::planner::plan_from_goal_with_priming(
        &llm as &dyn PlannerLlm,
        &goal,
        &project.root,
        priming.as_deref(),
    )
    .await
    .context("planner call failed")?;

    eprintln!(
        "[pilot] proposed {} task(s) (run id: {run_id}).",
        plan.len()
    );
    eprint!("\n{}", render_plan(&plan));

    // J9 — surface a cost/time/risk estimate with confidence before the
    // approval decision. Derive per-role cost samples from prior runs'
    // recorded per-task spend so the bands tighten (and confidence rises)
    // once the project has history; with no history this gracefully falls
    // back to the static per-role priors (low confidence, wide bands).
    let cost_samples = wingman_autonomous::estimate::cost_samples_from_runs(
        wingman_autonomous::dashboard::load_all_run_states(&project.root).iter(),
    );
    let estimate = wingman_autonomous::estimate::estimate_plan(
        &plan,
        &cost_samples,
        pilot.max_concurrent_agents,
    );
    eprintln!("[pilot] {}", estimate.render().replace('\n', "\n[pilot] "));

    // E1 trust-tiered approval. Classifier decides whether to proceed
    // silently (auto), surface a veto window (notify-only), or fall
    // back to the y/e/n prompt (hard).
    let report =
        wingman_autonomous::approval::classify(wingman_autonomous::approval::ClassifyInputs {
            plan: &plan,
            config: &pilot.approval,
            tier: pilot.tier,
            force_auto: opts.yes,
            force_hard: opts.review,
            estimate: Some(&estimate),
        });
    // R1 reversibility enforcement: layer the per-tier reversibility
    // gate over E1's trust decision. An irreversible task always forces a
    // hard gate; a `hard`-reversibility task hard-gates on copilot and
    // drops auto→notify-only on autopilot. `final_approval_tier` is a
    // no-op when the plan carries no elevated reversibility.
    let effective_tier =
        wingman_autonomous::escalation::final_approval_tier(&plan, report.tier, pilot.tier);
    if effective_tier != report.tier {
        eprintln!(
            "[pilot] approval: {} → {} (R1 reversibility override)",
            report.tier, effective_tier
        );
    }
    eprintln!(
        "[pilot] approval: {} (est. ${:.2}) — {}",
        effective_tier, report.estimated_usd, report.reason
    );

    // Surface the gate in state.json so `pilot watch` shows AwaitingApproval
    // and `pilot approve` / `pilot veto` have something to act on.
    if !matches!(
        effective_tier,
        wingman_autonomous::approval::ApprovalTier::Auto
    ) {
        let _ = store
            .append(wingman_autonomous::model::Event::RunStatusEv {
                t: wingman_autonomous::RunStore::now(),
                status: wingman_autonomous::RunStatus::AwaitingApproval,
            })
            .await;
    }

    let approve = match effective_tier {
        wingman_autonomous::approval::ApprovalTier::Auto => true,
        wingman_autonomous::approval::ApprovalTier::NotifyOnly => {
            run_notify_window(
                &plan,
                &goal,
                pilot.approval.notify_only_window_secs,
                &pilot.approval.notify_channel,
                Some(&run_path),
            )
            .await?
        }
        wingman_autonomous::approval::ApprovalTier::Hard => {
            if std::io::stdin().is_terminal() {
                prompt_for_approval(&plan, &goal)?
            } else if opts.await_approval || detached_child {
                // Headless hard gate, opted in: wait for an approve/veto over
                // the control channel (`pilot approve` / `pilot veto` or the
                // watch UI). Denies by default when the window elapses.
                eprintln!(
                    "[pilot] hard gate, no TTY — awaiting approval via the control channel \
                     (`pilot approve` / `pilot veto`), up to {}s…",
                    opts.approval_timeout_secs
                );
                wait_for_approval(&run_path, opts.approval_timeout_secs).await
            } else {
                eprintln!("[pilot] hard-gate required and no TTY — refusing to auto-approve plan.");
                false
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

    // J11 — fail closed on the untrusted/irreversible ("vm") tier. Real
    // sandboxed worker execution isn't wired yet, so a vm-tier task
    // (migrations, infra, Dockerfile/terraform edits, or an irreversible
    // goal) would otherwise run with full host access. Refuse rather than
    // silently execute unsandboxed, unless the operator opted in. Container-
    // tier tasks still degrade to host (annotated post-run) — this gate only
    // guards the genuinely dangerous top tier.
    if !pilot.sandbox.allow_unsandboxed_vm_tasks {
        let default_tier =
            wingman_autonomous::sandbox::SandboxTier::parse(&pilot.sandbox.default_tier);
        let vm_tasks: Vec<String> = store
            .state()
            .tasks
            .iter()
            .filter(|t| {
                wingman_autonomous::sandbox::select_tier(t, default_tier)
                    == wingman_autonomous::sandbox::SandboxTier::Vm
            })
            .map(|t| t.id.clone())
            .collect();
        if !vm_tasks.is_empty() {
            eprintln!(
                "[pilot] refusing to run: {} task(s) need vm-tier isolation \
                 (migrations / infra / irreversible / untrusted) but sandboxed \
                 execution isn't available — they would run unsandboxed on the host:",
                vm_tasks.len()
            );
            for id in &vm_tasks {
                eprintln!("[pilot]   - {id}");
            }
            eprintln!(
                "[pilot] relabel/split the task, or set [pilot.sandbox].\
                 allow_unsandboxed_vm_tasks = true to accept host execution."
            );
            return Ok(ExitCode::from(2));
        }
    }

    let base_branch = std::env::var("WINGMAN_PILOT_BASE_BRANCH").unwrap_or_else(|_| "main".into());
    let orch_cfg = wingman_autonomous::orchestrator::OrchestratorConfig {
        max_concurrent_agents: pilot.max_concurrent_agents,
        task_timeout: std::time::Duration::from_secs(pilot.task_timeout_secs),
        project_root: project.root.clone(),
        run_id: run_id.clone(),
        base_commit: base_commit.clone(),
        use_real_worktrees: true,
        max_usd: pilot.max_usd,
        max_retries_per_task: 1,
        enforce_checkpoint_hygiene: capability_on(&pilot, "checkpoint_hygiene"),
    };
    let stats_path = wingman_config::global_dir()
        .ok()
        .map(|g| g.join("stats.jsonl"));
    let routing = load_routing_aggregates(stats_path.as_deref());
    let inputs = wingman_autonomous::pipeline::PipelineInputs {
        provider,
        manager_model: selection.model.clone(),
        worker_spawner: build_real_worker_spawner(
            pilot.worker_model.as_deref().unwrap_or(&selection.model),
            &selection.model,
            routing,
            std::time::Duration::from_secs(pilot.task_timeout_secs),
        )?,
        base_branch,
        project_root: project.root.clone(),
        command_runner: Box::new(wingman_autonomous::pr::SystemCommandRunner),
        no_pr: opts.no_pr,
        orchestrator_cfg: orch_cfg,
        max_ticks: 64,
        tier: pilot.tier,
        worker_model: pilot
            .worker_model
            .clone()
            .unwrap_or_else(|| selection.model.clone()),
        stats_path,
        auto_approved: effective_tier == wingman_autonomous::approval::ApprovalTier::Auto,
        pr_config: pilot.pr.clone(),
        security_config: pilot.security.clone(),
        run_reviewer: capability_on(&pilot, "per_task_reviewer"),
        run_critic: capability_on(&pilot, "critic"),
        reviewer_model: pilot
            .default_model
            .clone()
            .unwrap_or_else(|| selection.model.clone()),
        sandbox_default_tier: pilot.sandbox.default_tier.clone(),
        dangerous_paths: pilot.approval.dangerous_paths.clone(),
    };

    eprintln!(
        "[pilot] driving manager loop ({} ticks max)…",
        inputs.max_ticks
    );
    // E12 — `--watch` tails the in-process run with a compact, in-place
    // progress line. The pipeline future and the tail loop share one task
    // (via select!), so there are no Send bounds to satisfy and the tail
    // stops the instant the pipeline returns.
    let outcome = if opts.watch {
        run_with_watch(
            wingman_autonomous::pipeline::run_to_completion(store, inputs),
            &run_path,
        )
        .await
    } else {
        wingman_autonomous::pipeline::run_to_completion(store, inputs).await
    }
    .context("pipeline run_to_completion")?;
    // J5 — push a proactive status report (routed by R5). Best-effort: a
    // notification failure must not change the run's exit status.
    if let Ok(final_store) = RunStore::load(&run_path).await {
        report_run_outcome(
            &project.root,
            final_store.state(),
            &pilot.notifications,
            !outcome.failed_tasks.is_empty(),
        );
        // Per-phase token breakdown — the "where did the tokens go?" baseline.
        if let Ok(events) = final_store.read_events().await {
            eprintln!(
                "[pilot] {}",
                wingman_autonomous::reporting::render_token_breakdown(&events)
                    .replace('\n', "\n[pilot] ")
            );
        }
    }
    if !outcome.failed_tasks.is_empty() {
        eprintln!(
            "[pilot] some tasks did not reach Done: {:?}",
            outcome.failed_tasks
        );
        if let Some(packet) = &outcome.escalation_packet {
            eprintln!("[pilot] escalation packet written: {}", packet.display());
        }
        return Ok(ExitCode::from(2));
    }
    if let Some(pr) = outcome.pr {
        eprintln!("[pilot] PR opened: {}", pr.url);
    } else if outcome.merged.is_some() {
        eprintln!("[pilot] integration branch ready; PR step skipped (--no-pr).");
    }
    Ok(ExitCode::SUCCESS)
}

/// E12 — drive the pipeline future while tailing the run with a compact,
/// in-place progress line. The future and the tail share one task (via
/// `select!`), so the tail stops the moment the pipeline returns and there
/// are no `Send` bounds to satisfy.
async fn run_with_watch<F>(
    fut: F,
    run_path: &std::path::Path,
) -> Result<
    wingman_autonomous::pipeline::PipelineOutcome,
    wingman_autonomous::pipeline::PipelineError,
>
where
    F: std::future::Future<
        Output = Result<
            wingman_autonomous::pipeline::PipelineOutcome,
            wingman_autonomous::pipeline::PipelineError,
        >,
    >,
{
    use std::time::Duration;
    tokio::pin!(fut);
    loop {
        tokio::select! {
            res = &mut fut => {
                eprintln!(); // end the in-place progress line
                return res;
            }
            _ = tokio::time::sleep(Duration::from_millis(750)) => {
                if let Ok(state) = wingman_autonomous::dashboard::load_state(run_path) {
                    let total = state.tasks.len();
                    let done = state
                        .tasks
                        .iter()
                        .filter(|t| t.status == wingman_autonomous::TaskStatus::Done)
                        .count();
                    let running = state
                        .tasks
                        .iter()
                        .filter(|t| t.status == wingman_autonomous::TaskStatus::InProgress)
                        .count();
                    eprint!(
                        "\r[pilot watch] {done}/{total} done · {running} running · ${:.2}   ",
                        state.totals.usd
                    );
                    std::io::stderr().flush().ok();
                }
            }
        }
    }
}

/// J5 + R5 — emit a proactive status report for a finished run, routed by
/// severity through `[pilot.notifications]`. `Immediate` channels are
/// delivered to the terminal (the always-available channel; Slack/email
/// transports are a deferred leaf that needs live accounts); `Digest`
/// notifications are appended to `<project>/.wingman/pilot-digest.jsonl` for
/// a later flush; `Suppress` drops silently.
fn report_run_outcome(
    project_root: &std::path::Path,
    state: &wingman_autonomous::RunState,
    cfg: &wingman_config::PilotNotificationsConfig,
    failed: bool,
) {
    use wingman_autonomous::notify::{route, NotificationSeverity, RoutingDecision};
    let (severity, body) = if failed {
        (
            NotificationSeverity::Escalation,
            wingman_autonomous::reporting::render_run_failure(state, "tasks did not reach Done"),
        )
    } else {
        (
            NotificationSeverity::Progress,
            wingman_autonomous::reporting::render_run_complete(state),
        )
    };
    match route(severity, cfg) {
        RoutingDecision::Immediate(channels) => {
            // Only the terminal is actually delivered today; Slack/email
            // transports need live accounts (a deferred leaf). Print the
            // notice to the terminal and, if the routing targeted channels we
            // can't yet deliver to, say so plainly rather than implying we
            // sent it there.
            eprintln!("[pilot] 🔔 {body}");
            let undelivered: Vec<&String> = channels
                .iter()
                .filter(|c| !matches!(c.as_str(), "desktop" | "terminal"))
                .collect();
            if !undelivered.is_empty() {
                eprintln!(
                    "[pilot]    (routed to {undelivered:?}, but those transports aren't wired yet — shown here instead)"
                );
            }
        }
        RoutingDecision::Digest => {
            let path = project_root.join(".wingman").join("pilot-digest.jsonl");
            if let Err(e) = append_digest_line(&path, severity.as_str(), &body) {
                eprintln!("[pilot] failed to queue digest notification: {e}");
            } else {
                eprintln!("[pilot] queued completion notice to digest.");
            }
        }
        RoutingDecision::Suppress => {}
    }
}

/// Append one digested notification line for a later `flush`.
fn append_digest_line(path: &std::path::Path, severity: &str, body: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::json!({ "severity": severity, "body": body });
    writeln!(f, "{line}")
}

/// Resolve whether a pilot capability is on: an explicit
/// `[pilot.capabilities]` override wins; otherwise the tier default from
/// the plan's tier→capability matrix applies.
fn capability_on(pilot: &wingman_config::PilotConfig, key: &str) -> bool {
    if let Some(&v) = pilot.capabilities.get(key) {
        return v;
    }
    use wingman_config::PilotTier::*;
    match key {
        // Per-task reviewer (E7): on for copilot and autopilot.
        "per_task_reviewer" => matches!(pilot.tier, Copilot | Autopilot),
        // Mandatory checkpoint hygiene (E11): on for copilot and autopilot.
        "checkpoint_hygiene" => matches!(pilot.tier, Copilot | Autopilot),
        // Critic (J10): autopilot-only by default.
        "critic" => matches!(pilot.tier, Autopilot),
        // Goal refinement / negotiation (J1): autopilot-only by default.
        "goal_refinement" => matches!(pilot.tier, Autopilot),
        // Unknown capability defaults off.
        _ => false,
    }
}

/// Slice out the first balanced top-level JSON object from a chatty reply
/// (models sometimes wrap JSON in prose or code fences). Falls back to the
/// whole string when no clear object is found.
fn extract_first_json_object(s: &str) -> &str {
    match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if b > a => &s[a..=b],
        _ => s,
    }
}

/// J1 — run the refinement agent and act on its verdict. Returns the
/// effective goal to plan against (the original or a restated one), or
/// `None` if the user vetoed/declined and the run should abort.
///
/// The refinement *decision* logic lives in (and is unit-tested by)
/// [`wingman_autonomous::refine`]; this function is the live wiring: it
/// makes the LLM call, parses the report, and renders the interactive
/// negotiation. A failed/garbled agent call degrades gracefully to "plan
/// the original goal" — refinement must never wedge a run.
async fn refine_goal(
    provider: &dyn wingman_core::Provider,
    model: &str,
    original_goal: &str,
    pilot: &wingman_config::PilotConfig,
) -> Option<String> {
    use wingman_autonomous::refine::{decide, parse_refinement, RefineAction};

    const SYSTEM: &str = "You are a senior engineer refining a work request before it is \
        planned. Read the goal and reply with ONLY a JSON object: \
        {\"clarifying_questions\":[\"…\"],\"goal_restatement\":\"…\"|null,\
        \"restatement_confidence\":\"low|medium|high\",\
        \"challenges\":[{\"severity\":\"low|medium|high|critical\",\"message\":\"…\"}],\
        \"alternatives\":[{\"description\":\"…\",\"tradeoff\":\"…\"}]}. \
        Only ask questions whose answer would materially change the plan. \
        Restate only when the goal is ambiguous but inferable. Be terse.";

    eprintln!("[pilot] refining goal (J1)…");
    let llm = ProviderLlm {
        provider,
        model: model.to_string(),
        max_tokens: 1024,
    };
    let raw = match (&llm as &dyn PlannerLlm)
        .complete(SYSTEM.to_string(), format!("GOAL:\n{original_goal}"))
        .await
    {
        Ok(r) if !r.trim().is_empty() => r,
        _ => {
            eprintln!("[pilot] refinement: agent returned nothing; planning the goal as stated.");
            return Some(original_goal.to_string());
        }
    };
    let report = match parse_refinement(extract_first_json_object(&raw)) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("[pilot] refinement: unparseable report; planning the goal as stated.");
            return Some(original_goal.to_string());
        }
    };

    match decide(&report, &pilot.refine, original_goal) {
        RefineAction::Proceed { goal } => {
            if goal != original_goal {
                eprintln!("[pilot] refinement: proceeding with restated goal — {goal}");
            }
            Some(goal)
        }
        RefineAction::NotifyWindow { goal, note } => {
            eprintln!("[pilot] refinement: {note}");
            // Reuse the same veto window the approval gate uses.
            if run_notify_window(
                &[],
                &goal,
                pilot.approval.notify_only_window_secs,
                &pilot.approval.notify_channel,
                None,
            )
            .await
            .unwrap_or(true)
            {
                Some(goal)
            } else {
                None
            }
        }
        RefineAction::AskUser {
            questions,
            challenges,
            alternatives,
        } => ask_user_refinement(original_goal, &questions, &challenges, &alternatives),
    }
}

/// Render the J1 negotiation to the operator and collect a decision. On a
/// non-interactive session we cannot ask, so we conservatively abort — the
/// agent itself flagged this goal as needing human input.
fn ask_user_refinement(
    original_goal: &str,
    questions: &[String],
    challenges: &[String],
    alternatives: &[wingman_autonomous::refine::Alternative],
) -> Option<String> {
    for c in challenges {
        eprintln!("[pilot] ⚠️  challenge: {c}");
    }
    for q in questions {
        eprintln!("[pilot] ❓ {q}");
    }
    for a in alternatives {
        let tradeoff = if a.tradeoff.is_empty() {
            String::new()
        } else {
            format!(" ({})", a.tradeoff)
        };
        eprintln!("[pilot] 💡 alternative: {}{tradeoff}", a.description);
    }
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "[pilot] refinement needs input but there's no TTY — aborting. \
             Re-run with a clarified goal."
        );
        return None;
    }
    eprint!("Proceed with the original goal anyway? [y / N] ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(original_goal.to_string()),
        _ => None,
    }
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

/// Re-exec the current binary as a detached background process for `-d`.
///
/// The child re-runs the same `pilot run` invocation (minus `-d`) with its
/// stdio pointed at `<run_dir>/pilot.log`, in its own session (`setsid` on
/// Unix / `DETACHED_PROCESS` on Windows) so it survives this shell. The run id
/// is passed through so the child adopts it and the printed id matches the log.
fn spawn_detached(run_id: &str, run_path: &std::path::Path) -> Result<()> {
    use std::process::{Command, Stdio};

    std::fs::create_dir_all(run_path)
        .with_context(|| format!("creating run dir {}", run_path.display()))?;
    let log_path = run_path.join("pilot.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening log {}", log_path.display()))?;

    let exe = std::env::current_exe().context("resolving current executable")?;
    // Drop the detach flag so the child runs in the foreground path. `-d` and
    // `--detached` are standalone clap tokens, so an exact-match filter is
    // enough. ponytail: won't strip a bundled short flag like `-dv`; clap
    // doesn't bundle bools here, so that combination never reaches us.
    let args = std::env::args_os()
        .skip(1)
        .filter(|a| a != "-d" && a != "--detached");

    let mut cmd = Command::new(exe);
    cmd.args(args)
        .env("WINGMAN_DETACHED_CHILD", "1")
        .env("WINGMAN_RUN_ID", run_id)
        .stdin(Stdio::null())
        .stdout(log.try_clone().context("cloning log handle")?)
        .stderr(log);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe; runs post-fork / pre-exec.
        unsafe {
            cmd.pre_exec(|| {
                let _ = nix::unistd::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS (0x8) | CREATE_NEW_PROCESS_GROUP (0x200): no console,
        // not part of this shell's Ctrl+C group.
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }

    let child = cmd.spawn().context("spawning detached pilot run")?;
    println!(
        "[pilot] run {run_id} detached (pid {}, log: {})",
        child.id(),
        log_path.display()
    );
    println!("[pilot] watch:  wingman pilot watch {run_id}");
    println!("[pilot] stop:   wingman pilot abort {run_id}");
    Ok(())
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
    plan: &[wingman_autonomous::planner::PlannedTask],
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
fn open_plan_in_editor(plan: &[wingman_autonomous::planner::PlannedTask]) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| {
            if cfg!(target_os = "windows") {
                "notepad".into()
            } else {
                "vi".into()
            }
        });
    let tmp = std::env::temp_dir().join(format!("wingman-plan-{}.json", std::process::id()));
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
/// Run the notify-only veto window, returning `true` to proceed with the
/// plan and `false` to reject it.
///
/// Decision sources, whichever comes first: an interactive `Enter` (veto), a
/// control-file `approve` / `veto` command (when `control_dir` is set — this
/// is how `pilot approve` / `pilot veto` and the watch UI drive a headless
/// run), or the window elapsing (proceed).
async fn run_notify_window(
    plan: &[wingman_autonomous::planner::PlannedTask],
    goal: &str,
    window_secs: u64,
    channel: &str,
    control_dir: Option<&std::path::Path>,
) -> Result<bool> {
    use std::time::{Duration, Instant};
    let _ = goal;
    let count = plan.len();
    eprintln!(
        "[pilot] notify-only: {count} tasks in plan, vetoing window {window_secs}s (channel: {channel})."
    );
    eprintln!(
        "[pilot] press Enter to veto, or from another terminal run `pilot approve` / `pilot veto`; ignore to proceed."
    );

    let window = Duration::from_secs(window_secs);
    let start = Instant::now();
    let mut reader = control_dir.map(|_| wingman_autonomous::control::ControlReader::new());

    // Poll the control file (if any) for an approve/veto decision.
    let poll_control = |reader: &mut Option<wingman_autonomous::control::ControlReader>| {
        use wingman_autonomous::control::ControlCommand;
        let (Some(r), Some(d)) = (reader.as_mut(), control_dir) else {
            return None;
        };
        for cmd in r.poll(d) {
            match cmd {
                ControlCommand::Approve => {
                    eprintln!("[pilot] approval received via control channel; proceeding.");
                    return Some(true);
                }
                ControlCommand::Veto => {
                    eprintln!("[pilot] veto received via control channel; rejecting plan.");
                    return Some(false);
                }
                _ => {}
            }
        }
        None
    };

    if std::io::stdin().is_terminal() {
        // Interactive: race a single stdin line against control-file polling
        // and the timeout.
        let read_line = tokio::task::spawn_blocking(|| {
            let mut buf = String::new();
            let _ = std::io::stdin().read_line(&mut buf);
            buf
        });
        tokio::pin!(read_line);
        loop {
            if let Some(decision) = poll_control(&mut reader) {
                return Ok(decision);
            }
            let Some(remaining) = window.checked_sub(start.elapsed()) else {
                eprintln!("[pilot] notify window elapsed; proceeding.");
                return Ok(true);
            };
            let tick = remaining.min(Duration::from_millis(250));
            tokio::select! {
                _ = &mut read_line => {
                    eprintln!("[pilot] veto received; rejecting plan.");
                    return Ok(false);
                }
                _ = tokio::time::sleep(tick) => {}
            }
        }
    } else {
        // Non-interactive (headless): poll the control file until a decision
        // or the window elapses. An operator can still SIGTERM.
        loop {
            if let Some(decision) = poll_control(&mut reader) {
                return Ok(decision);
            }
            if start.elapsed() >= window {
                eprintln!("[pilot] notify window elapsed; proceeding.");
                return Ok(true);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// Block a headless hard-gate run until an operator approves or vetoes the
/// plan over the control channel, or the window elapses.
///
/// Unlike the notify-only window, a hard gate **denies by default**: if no
/// decision arrives before the timeout, the plan is rejected. So CI that
/// forgets to approve fails closed rather than proceeding unsupervised.
async fn wait_for_approval(run_dir: &std::path::Path, timeout_secs: u64) -> bool {
    use std::time::{Duration, Instant};
    let mut reader = wingman_autonomous::control::ControlReader::new();
    let window = Duration::from_secs(timeout_secs);
    let start = Instant::now();
    loop {
        for cmd in reader.poll(run_dir) {
            match cmd {
                wingman_autonomous::control::ControlCommand::Approve => {
                    eprintln!("[pilot] approval received via control channel; proceeding.");
                    return true;
                }
                wingman_autonomous::control::ControlCommand::Veto => {
                    eprintln!("[pilot] veto received via control channel; rejecting plan.");
                    return false;
                }
                _ => {}
            }
        }
        if start.elapsed() >= window {
            eprintln!("[pilot] approval window elapsed with no decision; rejecting (deny-by-default).");
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ----------------------------------------------------------------------
// `wingman pilot resume`
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
    let run_path = wingman_autonomous::run_dir(&project.root, &run_id);
    if !run_path.exists() {
        return Err(anyhow!(
            "no run directory at {} — run id {run_id} not found",
            run_path.display()
        ));
    }

    let mut store = wingman_autonomous::RunStore::load(&run_path)
        .await
        .with_context(|| format!("loading run {run_id}"))?;

    let stuck = wingman_autonomous::pipeline::mark_stale_in_progress_failed(&mut store)
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
    if let Err(why) = wingman_autonomous::provider_support::gate_run(&selection.provider_id) {
        return Err(anyhow!(why));
    }
    let provider = runtime::build_provider(&cfg, &selection.provider_id)
        .with_context(|| format!("building provider {}", selection.provider_id))?;

    let state = store.state().clone();
    let base_branch = std::env::var("WINGMAN_PILOT_BASE_BRANCH").unwrap_or_else(|_| "main".into());
    let orch_cfg = wingman_autonomous::orchestrator::OrchestratorConfig {
        max_concurrent_agents: cfg.pilot.max_concurrent_agents,
        task_timeout: std::time::Duration::from_secs(cfg.pilot.task_timeout_secs),
        project_root: project.root.clone(),
        run_id: run_id.clone(),
        base_commit: state.base_commit.clone(),
        use_real_worktrees: true,
        max_usd: cfg.pilot.max_usd,
        max_retries_per_task: 1,
        enforce_checkpoint_hygiene: capability_on(&cfg.pilot, "checkpoint_hygiene"),
    };
    let stats_path = wingman_config::global_dir()
        .ok()
        .map(|g| g.join("stats.jsonl"));
    let routing = load_routing_aggregates(stats_path.as_deref());
    let inputs = wingman_autonomous::pipeline::PipelineInputs {
        provider,
        manager_model: selection.model.clone(),
        worker_spawner: build_real_worker_spawner(
            cfg.pilot
                .worker_model
                .as_deref()
                .unwrap_or(&selection.model),
            &selection.model,
            routing,
            std::time::Duration::from_secs(cfg.pilot.task_timeout_secs),
        )?,
        base_branch,
        project_root: project.root,
        command_runner: Box::new(wingman_autonomous::pr::SystemCommandRunner),
        no_pr,
        orchestrator_cfg: orch_cfg,
        max_ticks: 64,
        tier: cfg.pilot.tier,
        worker_model: cfg
            .pilot
            .worker_model
            .clone()
            .unwrap_or_else(|| selection.model.clone()),
        stats_path,
        // Resumed runs don't re-run the approval gate; be conservative and
        // don't auto-merge unless the operator re-approves.
        auto_approved: false,
        pr_config: cfg.pilot.pr.clone(),
        security_config: cfg.pilot.security.clone(),
        run_reviewer: capability_on(&cfg.pilot, "per_task_reviewer"),
        run_critic: capability_on(&cfg.pilot, "critic"),
        reviewer_model: cfg
            .pilot
            .default_model
            .clone()
            .unwrap_or_else(|| selection.model.clone()),
        sandbox_default_tier: cfg.pilot.sandbox.default_tier.clone(),
        dangerous_paths: cfg.pilot.approval.dangerous_paths.clone(),
    };

    eprintln!("[pilot] resume: driving manager loop for run {run_id}");
    let outcome = wingman_autonomous::pipeline::run_to_completion(store, inputs)
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

/// Build the production WorkerSpawner: spawns real `wingman --worker-mode`
/// child processes via [`wingman_autonomous::worker::run_worker`].
///
/// `manager_model` is the bigger model the orchestrator escalates to on
/// rung 2 of the E5 retry ladder. `worker_model` is the cheaper default.
///
/// `routing` carries the E6 cross-run stats (aggregated from
/// `stats.jsonl`). When present, the base (non-escalated) worker model is
/// chosen adaptively per role: a role whose cheap-model history is below
/// threshold is dispatched straight to the capable model instead of
/// burning a first attempt that history says will fail.
fn build_real_worker_spawner(
    worker_model: &str,
    manager_model: &str,
    routing: Option<std::sync::Arc<wingman_autonomous::learning::Aggregates>>,
    task_timeout: std::time::Duration,
) -> Result<wingman_autonomous::orchestrator::WorkerSpawner> {
    let wingman_bin = std::env::current_exe().context("locating wingman binary")?;
    let worker_model = worker_model.to_string();
    let manager_model = manager_model.to_string();
    Ok(std::sync::Arc::new(
        move |ctx: wingman_autonomous::orchestrator::SpawnContext| {
            let wingman_bin = wingman_bin.clone();
            let worker_model = worker_model.clone();
            let manager_model = manager_model.clone();
            let routing = routing.clone();
            Box::pin(async move {
                // E5 rung 2: escalate to the manager model when the
                // orchestrator flagged this attempt as needing it. Otherwise
                // E6 adaptive routing picks the base model per role.
                let model = if ctx.escalate_model {
                    Some(manager_model)
                } else if let Some(agg) = &routing {
                    Some(wingman_autonomous::learning::route_model(
                        agg,
                        ctx.task.role.as_str(),
                        &worker_model,
                        &manager_model,
                        ROUTE_SUCCESS_THRESHOLD,
                        ROUTE_MIN_SAMPLES,
                    ))
                } else {
                    Some(worker_model)
                };
                // Splice prior-failure history into the task's goal so the
                // next worker sees what went wrong. Cheap context augment;
                // E11 checkpoint integration is the heavier sibling.
                let mut task = ctx.task.clone();
                if !ctx.failure_history.is_empty() {
                    task.goal
                        .push_str("\n\n## Prior attempts on this task failed:\n");
                    for f in &ctx.failure_history {
                        task.goal.push_str(&format!("- {f}\n"));
                    }
                    task.goal.push_str(
                        "Read the failure context, fix the underlying issue, \
                     and re-run `run_acceptance` until every check is green \
                     before reporting `task_complete`.\n",
                    );
                }
                // E10 — take the manager→worker command receiver so
                // run_worker can drain it into the child's stdin.
                let cmd_rx = ctx.cmd_rx.lock().await.take();
                let spec = wingman_autonomous::worker::WorkerSpec {
                    wingman_bin,
                    role: task.role.clone(),
                    task,
                    worktree: ctx.worktree.clone(),
                    session_id: ctx.session_id.clone(),
                    model,
                    timeout: task_timeout,
                    cmd_rx,
                };
                // Pass the shared store by reference; run_worker locks it only
                // per event append, so workers actually run concurrently
                // instead of serializing on a guard held for the whole run.
                let result =
                    wingman_autonomous::worker::run_worker(&ctx.store, &ctx.agent_id, spec)
                        .await
                        .map_err(|e| {
                            wingman_autonomous::orchestrator::OrchestratorError::Spawn(
                                e.to_string(),
                            )
                        })?;
                Ok(wingman_autonomous::orchestrator::WorkerSpawnResult {
                    agent_id: ctx.agent_id,
                    status: result.status,
                    outcome: result.outcome,
                })
            })
        },
    ))
}

// ----------------------------------------------------------------------
// `wingman pilot status` and `wingman pilot watch`
// ----------------------------------------------------------------------

/// One-shot dashboard print. Picks the most recently updated run unless
/// the user names one. Exits non-zero if no runs exist under
/// `<project>/.wingman/autonomous/`.
pub async fn status(run_id: Option<String>) -> Result<ExitCode> {
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let runs = wingman_autonomous::dashboard::list_runs(&project.root).context("listing runs")?;
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
    let state = wingman_autonomous::dashboard::load_state(&pick.dir)?;
    let recent = wingman_autonomous::dashboard::tail_events(&pick.dir, 12)?;
    let view = wingman_autonomous::dashboard::render_dashboard(&state, &recent);
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
pub async fn watch(run_id: Option<String>, interval_ms: u64, ascii: bool) -> Result<ExitCode> {
    use std::time::Duration;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let runs = wingman_autonomous::dashboard::list_runs(&project.root)?;
    if runs.is_empty() {
        eprintln!("[pilot] no runs found under {}", project.root.display());
        return Ok(ExitCode::from(1));
    }
    // Validate an explicit --run-id up front so a typo fails fast rather
    // than silently watching the newest run.
    if let Some(id) = &run_id {
        if !runs.iter().any(|r| &r.run_id == id) {
            return Err(anyhow!("no run with id {id} found"));
        }
    }
    let pick = match &run_id {
        Some(id) => runs.iter().find(|r| &r.run_id == id).cloned().unwrap(),
        None => runs.into_iter().next().unwrap(),
    };

    // Interactive full-screen grid UI when attached to a terminal; fall
    // back to the pipe-friendly reprint loop otherwise (CI, `| tee`, logs).
    // The TUI manages the run list itself so it can offer a Runs sidebar
    // when several runs are active.
    if std::io::stdout().is_terminal() {
        let root = project.root.clone();
        return tokio::task::spawn_blocking(move || {
            crate::commands::pilot_watch_tui::run(&root, run_id, interval_ms, ascii)
        })
        .await
        .context("pilot watch UI task panicked")?;
    }

    eprintln!("[pilot] watching {} (Ctrl-C to exit)", pick.dir.display());

    let interval = Duration::from_millis(interval_ms.max(50));
    let mut last_mtime = None;
    loop {
        let mtime = wingman_autonomous::dashboard::state_mtime(&pick.dir);
        if mtime != last_mtime {
            last_mtime = mtime;
            match (
                wingman_autonomous::dashboard::load_state(&pick.dir),
                wingman_autonomous::dashboard::tail_events(&pick.dir, 12),
            ) {
                (Ok(state), Ok(recent)) => {
                    // Clear screen between frames with the ANSI sequence;
                    // plain enough to work on Windows console + cmd, gnome-
                    // terminal, kitty, iTerm without dragging in crossterm
                    // raw-mode plumbing.
                    print!("\x1b[2J\x1b[H");
                    let view = wingman_autonomous::dashboard::render_dashboard(&state, &recent);
                    print!("{}", view.to_ascii());
                    if matches!(
                        state.status,
                        wingman_autonomous::RunStatus::Done
                            | wingman_autonomous::RunStatus::Failed
                            | wingman_autonomous::RunStatus::Aborted
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

/// J2 — always-on discovery daemon. Polls the configured sources each
/// cycle (via the tested `daemon::run_cycle`), logs each candidate's
/// auto-run / propose / ignore decision, and appends accepted candidates
/// to `<project>/.wingman/daemon-queue.jsonl` for follow-up. `cycles == 0`
/// runs forever (Ctrl-C to stop); a positive value runs that many cycles
/// then exits (used for one-shot triage / CI).
pub async fn daemon(cfg: Config, cycles: usize) -> Result<ExitCode> {
    use std::time::Duration;

    let pilot = &cfg.pilot;
    if !pilot.daemon.enabled && cycles == 0 {
        eprintln!(
            "[pilot] daemon is disabled. Set `[pilot.daemon].enabled = true` in config, \
             or pass `--cycles N` for a one-shot discovery pass."
        );
        return Ok(ExitCode::from(1));
    }

    // Honesty check: only `github_issues` has a live discovery path. Warn
    // (don't fail) for any configured source we can't actually poll, so the
    // daemon doesn't look broken when it silently finds nothing.
    const IMPLEMENTED_SOURCES: &[&str] = &["github_issues", "todos"];
    for s in &pilot.daemon.sources {
        if !IMPLEMENTED_SOURCES.contains(&s.as_str()) {
            eprintln!(
                "[pilot] daemon: source '{s}' is configured but not yet implemented — ignoring it \
                 (implemented: {IMPLEMENTED_SOURCES:?})."
            );
        }
    }

    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let runner = wingman_autonomous::pr::SystemCommandRunner;
    // Mirror the J2 default: propose at ~40% of the auto threshold.
    let propose_floor = pilot.daemon.auto_threshold * 0.4;
    let queue_path = project.root.join(".wingman").join("daemon-queue.jsonl");
    let interval = Duration::from_secs(pilot.daemon.poll_interval_secs.max(1));

    eprintln!(
        "[pilot] daemon starting (sources: {:?}, auto_threshold: {:.2}, interval: {}s){}",
        pilot.daemon.sources,
        pilot.daemon.auto_threshold,
        pilot.daemon.poll_interval_secs,
        if cycles == 0 {
            " — Ctrl-C to stop".to_string()
        } else {
            format!(" — {cycles} cycle(s)")
        }
    );

    // Durable dedup across cycles (and restarts): every candidate ever
    // queued is remembered by source+title, so the same issue isn't
    // re-queued or re-dispatched every poll.
    let mut seen: std::collections::HashSet<String> = load_queued_keys(&queue_path);

    let mut n = 0usize;
    loop {
        let results = wingman_autonomous::daemon::run_cycle(
            &runner,
            &project.root,
            &pilot.daemon,
            propose_floor,
        );
        if results.is_empty() {
            eprintln!("[pilot] daemon cycle {n}: no candidates");
        }
        for (cand, action) in &results {
            eprintln!(
                "[pilot] daemon cycle {n}: {:?} — {} (score {:.2}, {})",
                action,
                cand.title,
                cand.score(),
                cand.source
            );
            let key = format!("{}\u{1}{}", cand.source, cand.title);
            if !matches!(
                action,
                wingman_autonomous::daemon::DaemonAction::AutoRun
                    | wingman_autonomous::daemon::DaemonAction::Propose
            ) {
                continue;
            }
            if seen.contains(&key) {
                continue; // already handled in a prior cycle/run
            }
            seen.insert(key);
            if let Err(e) = append_daemon_queue(&queue_path, cand, *action) {
                eprintln!("[pilot] daemon: failed to queue candidate: {e}");
            }
            // J2 — auto-dispatch a trusted AutoRun candidate into a real
            // nested pilot run, if the operator opted in. Propose stays
            // queued for a human. Runs sequentially: one goal to completion
            // before the next.
            // ponytail: sequential dispatch honours "one at a time"; true
            // parallel nested runs (daemon.max_concurrent_runs) is future work.
            if pilot.daemon.auto_dispatch
                && matches!(action, wingman_autonomous::daemon::DaemonAction::AutoRun)
            {
                eprintln!("[pilot] daemon: auto-dispatching run for {:?}", cand.title);
                let opts = PilotOptions {
                    goal: cand.title.clone(),
                    yes: true, // trusted, already scored above threshold
                    ..PilotOptions::default()
                };
                match run(cfg.clone(), opts).await {
                    Ok(code) => {
                        eprintln!("[pilot] daemon: dispatched run exited {code:?}")
                    }
                    Err(e) => eprintln!("[pilot] daemon: dispatched run failed: {e}"),
                }
            }
        }

        n += 1;
        if cycles != 0 && n >= cycles {
            eprintln!("[pilot] daemon: completed {n} cycle(s), exiting.");
            return Ok(ExitCode::SUCCESS);
        }
        tokio::time::sleep(interval).await;
    }
}

/// R2 — post-merge feedback poll. Each cycle walks every recorded run that
/// opened a PR but has no recorded outcome yet, queries the PR's terminal
/// state via `gh`, and appends a `pr.outcome` event (merged / reverted /
/// hotfix-followed / closed) that the E6 cross-run learner later weights.
/// `cycles == 0` runs forever on `[pilot.daemon].poll_interval_secs`; a
/// positive value runs that many cycles then exits (CI / one-shot backfill).
pub async fn feedback(cfg: Config, cycles: usize) -> Result<ExitCode> {
    use std::time::Duration;

    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let runner = wingman_autonomous::pr::SystemCommandRunner;
    let interval = Duration::from_secs(cfg.pilot.daemon.poll_interval_secs.max(1));

    eprintln!(
        "[pilot] feedback poller starting (interval: {}s){}",
        cfg.pilot.daemon.poll_interval_secs,
        if cycles == 0 {
            " — Ctrl-C to stop".to_string()
        } else {
            format!(" — {cycles} cycle(s)")
        }
    );

    let mut n = 0usize;
    loop {
        let pending = feedback_pending_runs(&project.root).await;
        if pending.is_empty() {
            eprintln!("[pilot] feedback cycle {n}: no open PRs awaiting outcome");
        }
        for (dir, pr_url) in pending {
            let mut store = match wingman_autonomous::store::RunStore::load(&dir).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[pilot] feedback: load {} failed: {e}", dir.display());
                    continue;
                }
            };
            match wingman_autonomous::feedback::poll_and_record(
                &runner,
                &mut store,
                &project.root,
                &pr_url,
            )
            .await
            {
                Ok(Some(kind)) => {
                    eprintln!("[pilot] feedback: {pr_url} → {kind:?}")
                }
                Ok(None) => eprintln!("[pilot] feedback: {pr_url} still open"),
                Err(e) => eprintln!("[pilot] feedback: {pr_url} poll failed: {e}"),
            }
        }

        n += 1;
        if cycles != 0 && n >= cycles {
            eprintln!("[pilot] feedback: completed {n} cycle(s), exiting.");
            return Ok(ExitCode::SUCCESS);
        }
        tokio::time::sleep(interval).await;
    }
}

/// Runs that opened a PR (`pr_url` set) but have no `pr.outcome` event yet.
/// Skipping already-recorded runs is the whole idempotency story — otherwise
/// `poll_and_record` re-appends an outcome on every terminal poll.
async fn feedback_pending_runs(
    project_root: &std::path::Path,
) -> Vec<(std::path::PathBuf, String)> {
    let mut out = Vec::new();
    let Ok(runs) = wingman_autonomous::dashboard::list_runs(project_root) else {
        return out;
    };
    for r in runs {
        let Ok(state) = wingman_autonomous::dashboard::load_state(&r.dir) else {
            continue;
        };
        let Some(pr_url) = state.pr_url.clone() else {
            continue;
        };
        // Skip runs that already have a recorded outcome.
        let already = match wingman_autonomous::store::RunStore::load(&r.dir).await {
            Ok(s) => s
                .read_events()
                .await
                .map(|evs| {
                    evs.iter().any(|e| {
                        matches!(e, wingman_autonomous::model::Event::PrOutcome { .. })
                    })
                })
                .unwrap_or(false),
            Err(_) => false,
        };
        if !already {
            out.push((r.dir, pr_url));
        }
    }
    out
}

/// J12 — install the skill packs listed in `[pilot.skills].packs`. Each
/// `owner/name@version` spec is fetched from `https://github.com/owner/name`
/// (tag `v<version>`) into `~/.wingman/packs/<slug>/` and its role/lessons
/// files are copied into `~/.wingman/agents/` so the role loader picks them
/// up. Already-installed packs (exact version present) only re-install files.
pub async fn skills_install(cfg: Config) -> Result<ExitCode> {
    use wingman_autonomous::skillpack;
    let (refs, errs) = skillpack::parse_pack_list(&cfg.pilot.skills.packs);
    for e in &errs {
        eprintln!("[pilot] skills: bad spec — {e}");
    }
    if refs.is_empty() {
        eprintln!("[pilot] skills: no valid packs in [pilot.skills].packs");
        return Ok(if errs.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        });
    }
    let home = wingman_config::global_dir()
        .ok()
        .and_then(|d| d.parent().map(|p| p.to_path_buf()))
        .or_else(dirs_home)
        .ok_or_else(|| anyhow!("cannot resolve home directory for pack install"))?;
    let runner = wingman_autonomous::pr::SystemCommandRunner;
    let mut failures = 0;
    for r in &refs {
        let url = format!("https://github.com/{}/{}", r.owner, r.name);
        match skillpack::fetch_pack(&runner, r, &url, &home) {
            Ok(dest) => eprintln!("[pilot] skills: installed {} → {}", r.slug(), dest.display()),
            Err(e) => {
                eprintln!("[pilot] skills: {} failed — {e}", r.slug());
                failures += 1;
            }
        }
    }
    Ok(if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Best-effort home dir from `$HOME` / `%USERPROFILE%` without pulling in the
/// `dirs` crate.
fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

/// R4 — eval / regression harness + CI gate.
///
/// Two modes:
/// - `--goals <FILE>` runs each goal line live through the pilot pipeline,
///   harvesting success/usd/wall, and writes `<eval>/results.jsonl`.
/// - otherwise reads an existing `<eval>/results.jsonl` (produced earlier or
///   hand-authored).
///
/// Then it summarizes, compares to `<eval>/baseline.json`, prints the
/// markdown report, and **exits non-zero on regression** — that exit code is
/// the CI gate. `--update-baseline` rewrites the baseline from the current
/// results and skips gating.
pub async fn eval(
    cfg: Config,
    goals_file: Option<std::path::PathBuf>,
    threshold: f64,
    update_baseline: bool,
) -> Result<ExitCode> {
    use wingman_autonomous::eval::EvalResult;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let eval_dir = project.root.join(".wingman").join("eval");
    let results_path = eval_dir.join("results.jsonl");
    let baseline_path = eval_dir.join("baseline.json");

    // Gather this run's results: live if --goals given, else from disk.
    let results: Vec<EvalResult> = if let Some(gf) = goals_file {
        let goals: Vec<String> = std::fs::read_to_string(&gf)
            .with_context(|| format!("reading goals file {}", gf.display()))?
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        if goals.is_empty() {
            eprintln!("[pilot] eval: no goals in {}", gf.display());
            return Ok(ExitCode::from(1));
        }
        eprintln!("[pilot] eval: running {} canned goal(s) live…", goals.len());
        let res = run_eval_goals(&cfg, &project.root, &goals).await;
        write_eval_results(&results_path, &res)?;
        res
    } else {
        read_eval_results(&results_path)?
    };

    if results.is_empty() {
        eprintln!(
            "[pilot] eval: no results (run with --goals <FILE>, or populate {})",
            results_path.display()
        );
        return Ok(ExitCode::from(1));
    }

    if update_baseline {
        write_eval_results(&baseline_path, &results)?;
        eprintln!(
            "[pilot] eval: baseline updated ({} result(s)) → {}",
            results.len(),
            baseline_path.display()
        );
        return Ok(ExitCode::SUCCESS);
    }

    let baseline = read_eval_results(&baseline_path).unwrap_or_default();
    let baseline_ref = if baseline.is_empty() {
        None
    } else {
        Some(baseline.as_slice())
    };
    let (report, regressed) = eval_gate(&results, baseline_ref, threshold);
    print!("{report}");
    if regressed {
        eprintln!("[pilot] eval: REGRESSION detected — failing the gate.");
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

/// Pure CI-gate core: summarize current results, compare to baseline, render
/// the markdown report, and report whether the gate should fail. Split out
/// so the summarize→compare→gate wiring is unit-testable without any I/O.
fn eval_gate(
    current: &[wingman_autonomous::eval::EvalResult],
    baseline: Option<&[wingman_autonomous::eval::EvalResult]>,
    threshold: f64,
) -> (String, bool) {
    use wingman_autonomous::eval;
    let cur = eval::summarize(current);
    match baseline {
        None => (
            format!(
                "# Eval report\n\nNo baseline to compare against. {} result(s), \
                 {:.0}% success, avg ${:.2}.\n\nRun `wingman pilot eval --update-baseline` \
                 to set one.\n",
                cur.n,
                cur.success_rate * 100.0,
                cur.avg_usd
            ),
            false,
        ),
        Some(base) => {
            let b = eval::summarize(base);
            let report = eval::compare(&cur, &b, threshold);
            (eval::render_report(&report), report.regressed)
        }
    }
}

/// Run each canned goal live through the pilot pipeline, harvesting metrics
/// from the resulting run state. success = the run reached Done; usd from the
/// run's recorded totals; wall from the wall clock around the call.
/// ponytail: quality is success-proxied (1.0/0.0) — a real LLM-judge needs a
/// golden diff per goal, which doesn't exist yet. Add it when golden refs do.
async fn run_eval_goals(
    cfg: &Config,
    project_root: &std::path::Path,
    goals: &[String],
) -> Vec<wingman_autonomous::eval::EvalResult> {
    use std::time::Instant;
    use wingman_autonomous::eval::EvalResult;
    let mut out = Vec::with_capacity(goals.len());
    for goal in goals {
        let before: std::collections::HashSet<String> =
            wingman_autonomous::dashboard::list_runs(project_root)
                .unwrap_or_default()
                .into_iter()
                .map(|r| r.run_id)
                .collect();
        let started = Instant::now();
        let opts = PilotOptions {
            goal: goal.clone(),
            yes: true,
            ..PilotOptions::default()
        };
        let _ = run(cfg.clone(), opts).await;
        let wall_min = started.elapsed().as_secs_f64() / 60.0;

        // Find the run this goal produced (newest id not seen before) and
        // read its terminal status + spend.
        let (success, usd) = wingman_autonomous::dashboard::list_runs(project_root)
            .unwrap_or_default()
            .into_iter()
            .find(|r| !before.contains(&r.run_id))
            .and_then(|r| wingman_autonomous::dashboard::load_state(&r.dir).ok())
            .map(|s| (s.status == wingman_autonomous::RunStatus::Done, s.totals.usd))
            .unwrap_or((false, 0.0));

        out.push(EvalResult {
            goal: goal.clone(),
            success,
            usd,
            wall_min,
            quality: if success { 1.0 } else { 0.0 },
        });
        eprintln!(
            "[pilot] eval: {goal:?} → {} (${usd:.2}, {wall_min:.1}m)",
            if success { "ok" } else { "fail" }
        );
    }
    out
}

fn read_eval_results(
    path: &std::path::Path,
) -> Result<Vec<wingman_autonomous::eval::EvalResult>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(r) = serde_json::from_str(line) {
            out.push(r);
        }
    }
    Ok(out)
}

fn write_eval_results(
    path: &std::path::Path,
    results: &[wingman_autonomous::eval::EvalResult],
) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::File::create(path)?;
    for r in results {
        writeln!(f, "{}", serde_json::to_string(r)?)?;
    }
    Ok(())
}

/// Load the `source\x01title` keys already present in the daemon queue so a
/// restarted daemon doesn't re-queue or re-dispatch work it already handled.
/// Missing/unreadable queue → empty set (nothing seen yet).
fn load_queued_keys(path: &std::path::Path) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let Ok(content) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in content.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            let source = v.get("source").and_then(|s| s.as_str()).unwrap_or("");
            let title = v.get("title").and_then(|s| s.as_str()).unwrap_or("");
            out.insert(format!("{source}\u{1}{title}"));
        }
    }
    out
}

/// Append one accepted daemon candidate to the queue log.
fn append_daemon_queue(
    path: &std::path::Path,
    cand: &wingman_autonomous::daemon::Candidate,
    action: wingman_autonomous::daemon::DaemonAction,
) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::json!({
        "source": cand.source,
        "title": cand.title,
        "score": cand.score(),
        "action": format!("{action:?}"),
    });
    writeln!(f, "{line}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wingman_autonomous::control::{append, ControlCommand};
    use std::time::Duration;

    #[test]
    fn r4_eval_gate_flags_regression_and_passes_on_parity() {
        use wingman_autonomous::eval::EvalResult;
        let good = |g: &str| EvalResult {
            goal: g.into(),
            success: true,
            usd: 0.10,
            wall_min: 1.0,
            quality: 1.0,
        };
        let baseline = vec![good("a"), good("b")];

        // no baseline → never gates
        let (_r, fail) = eval_gate(&baseline, None, 0.10);
        assert!(!fail);

        // parity → no regression
        let (_r, fail) = eval_gate(&baseline, Some(&baseline), 0.10);
        assert!(!fail);

        // success rate halved → regression
        let mut worse = baseline.clone();
        worse[0].success = false;
        let (report, fail) = eval_gate(&worse, Some(&baseline), 0.10);
        assert!(fail, "halved success rate must fail the gate");
        assert!(report.contains("REGRESSED"));

        // baseline round-trips through disk
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("baseline.json");
        write_eval_results(&p, &baseline).unwrap();
        assert_eq!(read_eval_results(&p).unwrap(), baseline);
    }

    #[tokio::test]
    async fn r2_feedback_pending_skips_norpr_and_already_recorded() {
        use wingman_autonomous::model::{Event, PrOutcomeKind};
        use wingman_autonomous::store::RunStore;
        let dir = tempfile::tempdir().unwrap();
        let auto = dir.path().join(".wingman").join("autonomous");

        async fn seed(auto: &std::path::Path, id: &str, pr: Option<&str>, recorded: bool) {
            let mut s = RunStore::create(auto.join(id), id, "g", "base", "wingman/auto")
                .await
                .unwrap();
            if let Some(url) = pr {
                s.append(Event::RunPr {
                    t: RunStore::now(),
                    url: url.into(),
                })
                .await
                .unwrap();
            }
            if recorded {
                s.append(Event::PrOutcome {
                    t: RunStore::now(),
                    run_id: id.into(),
                    kind: PrOutcomeKind::Merged,
                    revert_sha: None,
                    hours_to_revert: None,
                    hotfix_pr: None,
                    hours_to_hotfix: None,
                })
                .await
                .unwrap();
            }
        }
        seed(&auto, "run-open", Some("https://gh/pr/1"), false).await; // included
        seed(&auto, "run-done", Some("https://gh/pr/2"), true).await; // skipped (recorded)
        seed(&auto, "run-nopr", None, false).await; // skipped (no PR)

        let pending = feedback_pending_runs(dir.path()).await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, "https://gh/pr/1");
    }

    #[test]
    fn j2_load_queued_keys_dedups_by_source_and_title() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon-queue.jsonl");
        // missing file → empty
        assert!(load_queued_keys(&path).is_empty());
        std::fs::write(
            &path,
            "{\"source\":\"github_issues\",\"title\":\"fix bug\",\"score\":0.9,\"action\":\"AutoRun\"}\n\
             {\"source\":\"todos\",\"title\":\"fix bug\",\"score\":0.5,\"action\":\"Propose\"}\n\
             garbage-line\n",
        )
        .unwrap();
        let keys = load_queued_keys(&path);
        assert_eq!(keys.len(), 2); // same title, different source ⇒ distinct
        assert!(keys.contains("github_issues\u{1}fix bug"));
        assert!(keys.contains("todos\u{1}fix bug"));
    }

    #[tokio::test]
    async fn wait_for_approval_returns_true_on_approve() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        // Approve arrives shortly after the wait starts.
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            append(&path, &ControlCommand::Approve).unwrap();
        });
        let approved = wait_for_approval(dir.path(), 5).await;
        writer.await.unwrap();
        assert!(approved, "approve command should release the gate");
    }

    #[tokio::test]
    async fn wait_for_approval_returns_false_on_veto() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &ControlCommand::Veto).unwrap();
        assert!(
            !wait_for_approval(dir.path(), 5).await,
            "veto rejects the plan"
        );
    }

    #[tokio::test]
    async fn wait_for_approval_denies_by_default_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        // No command written; a 0s window must reject rather than proceed.
        assert!(
            !wait_for_approval(dir.path(), 0).await,
            "a hard gate must fail closed when the window elapses"
        );
    }
}
