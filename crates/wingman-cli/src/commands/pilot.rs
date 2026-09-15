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

/// `pilot tell [run] [--task T] <message>` — inject a message into a live
/// worker's next turn.
pub async fn control_tell(
    run_id: Option<String>,
    task: Option<String>,
    message: String,
) -> Result<ExitCode> {
    send_control(
        run_id,
        wingman_autonomous::control::ControlCommand::Tell {
            task,
            message,
            reply: false,
        },
    )
}

/// `pilot ask [run] [--task T] <question>` — same delivery as `tell`, then
/// wait for the worker to answer.
///
/// The answer comes back the way every other worker→manager message does: as
/// a `worker_msg:` entry in the run's event log. Polling that beats opening a
/// second channel back to a CLI process that may not even be running by then.
pub async fn control_ask(
    run_id: Option<String>,
    task: Option<String>,
    message: String,
    wait_secs: u64,
) -> Result<ExitCode> {
    let pick = pick_run(run_id)?;
    // Count what is already there, so a stale answer from an earlier `ask`
    // isn't mistaken for this one's.
    let before = answer_count(&pick.dir);
    let cmd = wingman_autonomous::control::ControlCommand::Tell {
        task,
        message,
        reply: true,
    };
    wingman_autonomous::control::append(&pick.dir, &cmd)
        .with_context(|| format!("writing control command to run {}", pick.run_id))?;
    eprintln!("[pilot] {} -> {}", cmd.encode(), pick.run_id);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait_secs);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let answers = answers(&pick.dir);
        if answers.len() > before {
            for a in &answers[before..] {
                println!("{a}");
            }
            return Ok(ExitCode::SUCCESS);
        }
    }
    eprintln!(
        "[pilot] no answer within {wait_secs}s — the question was delivered; \
         the reply will show up in `wingman pilot watch {}`",
        pick.run_id
    );
    Ok(ExitCode::from(1))
}

/// Every `WorkerMessage::Answer` recorded in this run so far, oldest first.
fn answers(run_dir: &std::path::Path) -> Vec<String> {
    let Ok(events) = wingman_autonomous::dashboard::tail_events(run_dir, 5_000) else {
        return Vec::new();
    };
    events
        .iter()
        .filter_map(|ev| match ev {
            wingman_autonomous::model::Event::TaskTool { tool, .. } => {
                let line = tool.strip_prefix("worker_msg:")?;
                match wingman_autonomous::ipc::parse_message(line) {
                    Ok(Some(wingman_autonomous::ipc::WorkerMessage::Answer { text })) => Some(text),
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}

fn answer_count(run_dir: &std::path::Path) -> usize {
    answers(run_dir).len()
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
    /// Use this run id instead of minting one. The daemon's `pr_reviews`
    /// dispatch sets it so it can find the rework run afterwards.
    pub run_id: Option<String>,
    /// Stack the run on this existing branch instead of a fresh
    /// `wingman/auto/<run-id>`. Paired with `base` = the branch head and
    /// `no_pr`, the work lands as new commits on an open PR's branch.
    pub rework_branch: Option<String>,
}

pub async fn run(cfg: Config, opts: PilotOptions) -> Result<ExitCode> {
    // `-d`/`--detached`: if we're the top-level invocation (not the re-exec'd
    // child), mint the run id, spawn a detached copy of ourselves writing to
    // the run's log, print the id, and hand the shell back. The child re-enters
    // this function with WINGMAN_DETACHED_CHILD set and runs the pipeline.
    let detached_child = std::env::var_os("WINGMAN_DETACHED_CHILD").is_some();
    if opts.detached && !detached_child {
        let project = ProjectPaths::discover(&std::env::current_dir()?);
        // Honour a caller-supplied id here too, not just in the child below:
        // a caller that pre-minted one (`wingman board dispatch`) has already
        // recorded it, and minting a fresh one here would orphan that record.
        let run_id = resolve_run_id();
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
    // J10 — resolved before planning, so a critic that breaks
    // `critic_other_family` stops the run before any model is paid for.
    let critic = critic(
        &cfg,
        &pilot,
        pilot.worker_model.as_deref().unwrap_or(&selection.model),
        wingman_autonomous::pipeline::AuxAgent {
            provider: provider.clone(),
            model: selection.model.clone(),
        },
    )?;

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
    // Any repo you run pilot in shows up on the board. Best-effort and
    // deliberately silent — a board problem must never fail a run.
    crate::commands::board::touch(&project.root);
    let base_commit = resolve_base_commit(&project.root, opts.base.as_deref())?;
    // A detached child inherits the run id its parent minted (so the id it
    // prints and the log path it writes to line up with the child's run).
    let run_id = opts.run_id.clone().unwrap_or_else(resolve_run_id);
    let integration = opts
        .rework_branch
        .clone()
        .unwrap_or_else(|| integration_branch(&run_id));
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

    // `info` routes to `suppress` by default, so this needs both
    // `desktop_inbox = true` and `info = "desktop"` — opt-in twice, which is
    // right for the least actionable thing here. You started the run; being
    // told so is a notification for your own keystroke.
    if let Some(dir) = wingman_autonomous::notify::desktop_target(
        wingman_autonomous::notify::NotificationSeverity::Info,
        &pilot.notifications,
    ) {
        let _ = wingman_config::inbox::append_to(
            &dir,
            &wingman_config::inbox::Notification {
                project: project
                    .root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned()),
                run_dir: Some(run_path.display().to_string()),
                ..wingman_config::inbox::Notification::now("info", "Run started", &goal)
            },
        );
    }

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
    // J8 — and with what earlier merged runs left in the project's knowledge
    // layer: the architecture summary, recent decisions, merge hotspots.
    let priming = match (
        priming,
        wingman_autonomous::knowledge::render_planner_context(&project.root),
    ) {
        (Some(p), Some(k)) => Some(format!("{p}\n\n{k}")),
        (p, k) => p.or(k),
    };
    let mut plan = wingman_autonomous::planner::plan_from_goal_with_priming(
        &llm as &dyn PlannerLlm,
        &goal,
        &project.root,
        priming.as_deref(),
    )
    .await
    .context("planner call failed")?;

    // J10 — the critic reads the plan before anyone approves it; its medium+
    // risks become guardrail tasks that run after the plan's own.
    if let Some(critic) = &critic {
        eprintln!("[pilot] critic reviewing the plan ({})…", critic.model);
        let critic_llm = ProviderLlm {
            provider: critic.provider.as_ref(),
            model: critic.model.clone(),
            max_tokens: 4096,
        };
        match wingman_autonomous::critic::review_plan(&critic_llm, &goal, &plan).await {
            Some(report) => {
                let added = wingman_autonomous::critic::append_guardrails(&mut plan, &report);
                eprintln!(
                    "[pilot] critic: {} risk(s), {added} guardrail task(s) added.",
                    report.risks.len()
                );
            }
            None => eprintln!("[pilot] critic: no usable plan review; no guardrails added."),
        }
    }

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

    // How long the gate will wait, when `control.jsonl` is what releases it.
    //
    // `None` means a desktop Approve would be a dead button: the hard gate with
    // a TTY blocks on stdin and never reads the control file, and the last arm
    // refuses outright. The card is still worth showing in those cases — it
    // just says where to go rather than offering to decide. Keep this in step
    // with the `approve` match below.
    let gate_secs = match effective_tier {
        wingman_autonomous::approval::ApprovalTier::Auto => None,
        wingman_autonomous::approval::ApprovalTier::NotifyOnly => {
            Some(pilot.approval.notify_only_window_secs)
        }
        wingman_autonomous::approval::ApprovalTier::Hard => {
            if std::io::stdin().is_terminal() {
                None
            } else if opts.await_approval || detached_child {
                Some(opts.approval_timeout_secs)
            } else {
                None
            }
        }
    };

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

        // R5 `decision`: the first thing that has ever emitted this severity.
        // The buttons carry the literal `ControlCommand`, so the desktop app
        // appends to `control.jsonl` — the file `wait_for_approval` and
        // `run_notify_window` are already tailing — and this side of the gate
        // needs no new wait path at all.
        if let Some(dir) = wingman_autonomous::notify::desktop_target(
            wingman_autonomous::notify::NotificationSeverity::Decision,
            &pilot.notifications,
        ) {
            use wingman_autonomous::control::ControlCommand;
            use wingman_config::inbox::{Action, Notification};

            let button = |id: &str, label: &str, cmd: ControlCommand| Action {
                id: id.into(),
                label: label.into(),
                control: serde_json::to_value(cmd).ok(),
            };
            let card = Notification {
                project: project
                    .root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned()),
                run_dir: Some(run_path.display().to_string()),
                expires_at: gate_secs.map(|s| wingman_config::inbox::now_secs().saturating_add(s)),
                actions: match gate_secs {
                    Some(_) => vec![
                        button("approve", "Approve", ControlCommand::Approve),
                        button("veto", "Veto", ControlCommand::Veto),
                    ],
                    None => Vec::new(),
                },
                ..Notification::now(
                    "decision",
                    format!("Plan awaiting approval — {} tasks", plan.len()),
                    if gate_secs.is_some() {
                        format!(
                            "{goal}\n\nest. ${:.2} — {}",
                            report.estimated_usd, report.reason
                        )
                    } else {
                        format!(
                            "{goal}\n\nest. ${:.2} — {}\n\nWaiting for you in the terminal.",
                            report.estimated_usd, report.reason
                        )
                    },
                )
            };
            let _ = wingman_config::inbox::append_to(&dir, &card);
        }
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

    // J11 — probe once which sandbox tiers this machine can honour; the gate
    // below, the worker spawner and the run report all use this answer.
    let sandbox_avail = wingman_autonomous::sandbox::TierAvailability::probe(
        &pilot.sandbox,
        &wingman_autonomous::pr::SystemCommandRunner,
    );
    if refuse_unisolated_vm_tasks(&pilot.sandbox, &sandbox_avail, &store.state().tasks) {
        return Ok(ExitCode::from(2));
    }

    let base_branch =
        std::env::var("WINGMAN_PILOT_BASE_BRANCH").unwrap_or_else(|_| pilot.pr.base_branch.clone());
    let orch_cfg = wingman_autonomous::orchestrator::OrchestratorConfig {
        max_concurrent_agents: pilot.max_concurrent_agents,
        task_timeout: std::time::Duration::from_secs(pilot.task_timeout_secs),
        project_root: project.root.clone(),
        run_id: run_id.clone(),
        base_commit: base_commit.clone(),
        use_real_worktrees: true,
        max_usd: pilot.max_usd,
        max_total_tokens: pilot.max_total_tokens,
        max_retries_per_task: pilot.max_retries_per_task,
        desktop_inbox: wingman_autonomous::notify::desktop_target(
            wingman_autonomous::notify::NotificationSeverity::Escalation,
            &pilot.notifications,
        ),
        sample_host_load: capability_on(&pilot, "adaptive_concurrency"),
        speculative_prespawn: capability_on(&pilot, "speculative_prespawn"),
        warm_cmd: pilot.turn_gate_cmd.clone(),
    };
    let stats_path = wingman_config::global_dir()
        .ok()
        .map(|g| g.join("stats.jsonl"));
    let routing = load_routing_aggregates(stats_path.as_deref());
    let inputs = wingman_autonomous::pipeline::PipelineInputs {
        provider: provider.clone(),
        manager_model: selection.model.clone(),
        worker_spawner: build_real_worker_spawner(
            pilot.worker_model.as_deref().unwrap_or(&selection.model),
            &selection.model,
            routing,
            learned_routing(&cfg, &project.root),
            std::time::Duration::from_secs(pilot.task_timeout_secs),
            turn_rollback_after(&pilot),
            capability_on(&pilot, "checkpoint_hygiene"),
            pilot.sandbox.clone(),
            sandbox_avail.clone(),
            tool_synthesis_for(&pilot, &project.config_file),
        )?,
        base_branch,
        project_root: project.root.clone(),
        command_runner: Box::new(wingman_autonomous::pr::SystemCommandRunner),
        no_pr: opts.no_pr,
        orchestrator_cfg: orch_cfg,
        max_ticks: pilot.max_manager_ticks,
        tier: pilot.tier,
        worker_model: pilot
            .worker_model
            .clone()
            .unwrap_or_else(|| selection.model.clone()),
        stats_path,
        auto_approved: effective_tier == wingman_autonomous::approval::ApprovalTier::Auto,
        pr_config: pilot.pr.clone(),
        security_config: pilot.security.clone(),
        disabled_tools: cfg.tools.disabled_tools.clone(),
        run_reviewer: capability_on(&pilot, "per_task_reviewer"),
        critic,
        // Resolve through the same provider/model split the manager uses so a
        // `provider/model` config value (e.g. `openrouter/deepseek/…`) becomes
        // the bare model id the provider's API expects — the prefixed string
        // 400s ("not a valid model ID") and the agent silently fails.
        reviewer_model: match pilot
            .reviewer_model
            .clone()
            .or_else(|| pilot.default_model.clone())
        {
            Some(s) => runtime::resolve_selection(&cfg, Some(&s))
                .map(|sel| sel.model)
                .unwrap_or_else(|_| selection.model.clone()),
            None => selection.model.clone(),
        },
        sandbox_default_tier: pilot.sandbox.default_tier.clone(),
        sandbox_availability: sandbox_avail,
        dangerous_paths: pilot.approval.dangerous_paths.clone(),
        merge_fixer: capability_on(&pilot, "merge_fixer"),
        knowledge_keeper: knowledge_keeper(
            &cfg,
            &pilot,
            wingman_autonomous::pipeline::AuxAgent {
                provider,
                model: selection.model.clone(),
            },
        ),
    };

    eprintln!(
        "[pilot] driving manager loop ({} ticks max)…",
        inputs.max_ticks
    );
    // E12 — `--watch` tails the in-process run with a compact, in-place
    // progress line. The pipeline future and the tail loop share one task
    // (via select!), so there are no Send bounds to satisfy and the tail
    // stops the instant the pipeline returns.
    let result = if opts.watch {
        run_with_watch(
            wingman_autonomous::pipeline::run_to_completion(store, inputs),
            &run_path,
        )
        .await
    } else {
        wingman_autonomous::pipeline::run_to_completion(store, inputs).await
    };

    // Account for the run's tokens BEFORE propagating any error: the events
    // are persisted regardless of outcome, and a run that deadlocks still
    // bills. Roll the spend into ~/.wingman/usage.json so `wingman cost` and
    // the /usage modal see pilot runs, and print the per-phase breakdown. All
    // best-effort — instrumentation never changes the run's exit status.
    if let Ok(final_store) = RunStore::load(&run_path).await {
        if let Ok(events) = final_store.read_events().await {
            eprintln!(
                "[pilot] {}",
                wingman_autonomous::reporting::render_token_breakdown(&events)
                    .replace('\n', "\n[pilot] ")
            );
            let by_model = wingman_autonomous::reporting::tokens_by_model(&events);
            if !by_model.is_empty() {
                wingman_tui::usage_store::LifetimeUsage::load().save_merged(&by_model);
            }
        }
    }

    let outcome = result.context("pipeline run_to_completion")?;
    // J5 — push a proactive status report (routed by R5). Best-effort: a
    // notification failure must not change the run's exit status.
    if let Ok(final_store) = RunStore::load(&run_path).await {
        report_run_outcome(
            &project.root,
            final_store.state(),
            &pilot.notifications,
            !outcome.failed_tasks.is_empty(),
        );
    }
    // J15 — every hard trigger the run hit, live or at the PR gate.
    for trigger in &outcome.escalation_triggers {
        eprintln!(
            "[pilot] escalation: {} — {}",
            trigger.short_label(),
            trigger.render()
        );
    }
    // E11 — finished tasks that skipped checkpoints. With `checkpoint_hygiene`
    // on they were failed before review instead, so this is the advisory view.
    for (task, reason) in &outcome.checkpoint_violations {
        eprintln!("[pilot] checkpoint hygiene (advisory): {task}: {reason}");
    }
    // A packet with no failed task is a run blocked on a trigger (a refused
    // force-push), which is not a finished run either.
    if !outcome.failed_tasks.is_empty() || outcome.escalation_packet.is_some() {
        if !outcome.failed_tasks.is_empty() {
            eprintln!(
                "[pilot] some tasks did not reach Done: {:?}",
                outcome.failed_tasks
            );
        }
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
    // R6 — say which scanners ran; a skipped one is not a clean result.
    if let Some(security) = &outcome.security {
        eprintln!(
            "[pilot] security pass: {} finding(s)",
            security.findings.len()
        );
        for note in &security.notes {
            eprintln!("[pilot]   {note}");
        }
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
    // The card the `desktop` channel writes, when the inbox is on. Informational
    // by design: a run that is already over has nothing left to decide, so the
    // buttons that would fit here would all be "open the run", which the card
    // itself is.
    let inbox_dir = wingman_autonomous::notify::desktop_dir(cfg);
    let card = wingman_config::inbox::Notification {
        project: project_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned()),
        ..wingman_config::inbox::Notification::now(
            severity.as_str(),
            if failed { "Run failed" } else { "Run finished" },
            &body,
        )
    };

    match route(severity, cfg) {
        RoutingDecision::Immediate(channels) => {
            // Terminal delivery is always the desktop/terminal channel.
            eprintln!("[pilot] 🔔 {body}");
            // Deliver to any routed channel that has a configured webhook URL
            // (`[pilot.notifications.webhooks].<channel>`), POSTing the Slack
            // incoming-webhook `{"text": …}` shape. Channels without an
            // endpoint fall back to the terminal notice above.
            let runner = wingman_autonomous::pr::SystemCommandRunner;
            let report = wingman_autonomous::notify::deliver_to_channels(
                &runner,
                &channels,
                &cfg.webhooks,
                &body,
                inbox_dir.as_deref().map(|d| (d, &card)),
            );
            for ch in &report.delivered {
                eprintln!("[pilot]    → delivered to '{ch}'");
            }
            for (ch, err) in &report.failed {
                eprintln!("[pilot]    (channel '{ch}' webhook failed: {err})");
            }
            if !report.unconfigured.is_empty() {
                eprintln!(
                    "[pilot]    (channels {:?} have no webhook — set \
                     [pilot.notifications.webhooks].<channel>; shown above instead)",
                    report.unconfigured
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
    let line = serde_json::json!({ "severity": severity, "body": body });
    wingman_config::append_line(path, &line.to_string())
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
        // Mandatory checkpoint hygiene (E11): fails multi-file work that never
        // checkpointed, so autopilot-only by default.
        "checkpoint_hygiene" => matches!(pilot.tier, Autopilot),
        // Critic (J10): autopilot-only by default.
        "critic" => matches!(pilot.tier, Autopilot),
        // Goal refinement / negotiation (J1): autopilot-only by default.
        "goal_refinement" => matches!(pilot.tier, Autopilot),
        // Host-load-aware concurrency cap (E9): every tier. Rate limits and
        // budget burn narrow the cap regardless.
        "adaptive_concurrency" => true,
        // Speculative worktree pre-spawn (E9): on for copilot and autopilot.
        "speculative_prespawn" => matches!(pilot.tier, Copilot | Autopilot),
        // Per-turn rollback to the last green checkpoint (E5.5): discards
        // worker edits, so autopilot-only by default.
        "turn_rollback" => matches!(pilot.tier, Autopilot),
        // Merge-fixer workers on an unresolved merge conflict (E4): copilot
        // and autopilot, with write-set scheduling.
        "merge_fixer" => matches!(pilot.tier, Copilot | Autopilot),
        // Knowledge-keeper agent after a merged run (J8): autopilot-only.
        "knowledge_keeper" => matches!(pilot.tier, Autopilot),
        // Tool synthesis (J7): autopilot-only by default.
        "tool_synthesis" => matches!(pilot.tier, Autopilot),
        // Unknown capability defaults off.
        _ => false,
    }
}

/// J8 — the knowledge-keeper pass, while its capability is on. It runs on the
/// `summarize` task class (`[router.classes]`, usually the fast model); when
/// that class is unrouted, or its provider cannot be built, it runs on
/// `manager` instead.
fn knowledge_keeper(
    cfg: &Config,
    pilot: &wingman_config::PilotConfig,
    manager: wingman_autonomous::pipeline::AuxAgent,
) -> Option<wingman_autonomous::pipeline::AuxAgent> {
    if !capability_on(pilot, "knowledge_keeper") {
        return None;
    }
    let routed = cfg
        .router
        .resolve_class("summarize")
        .and_then(|spec| cfg.resolve_model_spec(&spec))
        .and_then(
            |(provider_id, model)| match runtime::build_provider(cfg, &provider_id) {
                Ok(provider) => Some(wingman_autonomous::pipeline::AuxAgent { provider, model }),
                Err(e) => {
                    eprintln!(
                        "[pilot] knowledge-keeper: cannot build provider {provider_id} for the \
                         summarize class ({e}); using the manager model"
                    );
                    None
                }
            },
        );
    Some(routed.unwrap_or(manager))
}

/// J10 — the critic agent, while its capability is on. It runs on
/// `[pilot].critic_model`, else `reviewer_model`, else `default_model`, and
/// only without any of those on `manager`. With `critic_other_family` set, a
/// critic from `worker_model`'s family, or one whose family (either side) the
/// name does not tell, refuses the run: without that check the critic would
/// quietly share the workers' blind spots.
fn critic(
    cfg: &Config,
    pilot: &wingman_config::PilotConfig,
    worker_model: &str,
    manager: wingman_autonomous::pipeline::AuxAgent,
) -> Result<Option<wingman_autonomous::pipeline::AuxAgent>> {
    if !capability_on(pilot, "critic") {
        return Ok(None);
    }
    let spec = pilot
        .critic_model
        .as_ref()
        .or(pilot.reviewer_model.as_ref())
        .or(pilot.default_model.as_ref());
    let critic = match spec {
        Some(spec) => {
            let sel = runtime::resolve_selection(cfg, Some(spec))
                .with_context(|| format!("resolving the critic model {spec}"))?;
            let provider = runtime::build_provider(cfg, &sel.provider_id)
                .with_context(|| format!("building provider {} for the critic", sel.provider_id))?;
            wingman_autonomous::pipeline::AuxAgent {
                provider,
                model: sel.model,
            }
        }
        None => manager,
    };
    if pilot.critic_other_family {
        use wingman_autonomous::critic::model_family;
        match (model_family(&critic.model), model_family(worker_model)) {
            (Some(c), Some(w)) if c != w => {}
            (c, w) => {
                return Err(anyhow!(
                    "[pilot].critic_other_family is set, but the critic `{}` ({}) is not from \
                     another model family than the workers' `{worker_model}` ({}). Set \
                     [pilot].critic_model to a model from another family, or turn \
                     critic_other_family off.",
                    critic.model,
                    c.unwrap_or("unknown family"),
                    w.unwrap_or("unknown family"),
                ))
            }
        }
    }
    Ok(Some(critic))
}

/// E5.5 — the `--turn-rollback-after` a worker gets: `[pilot].turn_rollback_after`
/// while the `turn_rollback` capability is on, 0 (off) otherwise.
fn turn_rollback_after(pilot: &wingman_config::PilotConfig) -> u32 {
    if capability_on(pilot, "turn_rollback") {
        pilot.turn_rollback_after
    } else {
        0
    }
}

/// J7 — whether workers get `propose_tool`, and at which approval tier:
/// `None` when the `tool_synthesis` capability is off, otherwise
/// [`wingman_autonomous::approval::tool_synthesis_tier`] over the run's tier
/// and whether this project's config is trusted.
fn tool_synthesis_for(
    pilot: &wingman_config::PilotConfig,
    project_config: &std::path::Path,
) -> Option<wingman_autonomous::approval::ApprovalTier> {
    capability_on(pilot, "tool_synthesis").then(|| {
        wingman_autonomous::approval::tool_synthesis_tier(
            pilot.tier,
            wingman_config::trust::is_trusted(project_config),
        )
    })
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

/// The run id for this invocation: `WINGMAN_RUN_ID` when a caller supplied
/// one, else a fresh one.
///
/// Two callers set it. A detached parent passes its id to the child it
/// re-execs, so both halves agree on the log path. And `wingman board
/// dispatch` pre-mints an id so it can record the dispatch before the process
/// starts -- which only works if *every* entry point honours the variable,
/// including the detached parent.
fn resolve_run_id() -> String {
    std::env::var("WINGMAN_RUN_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(new_run_id)
}

/// Generate a run id of the form `YYYY-MM-DD-HHMM-<rand6>`.
fn new_run_id() -> String {
    use rand::distr::SampleString;
    let now = chrono::Utc::now();
    let suffix = rand::distr::Alphanumeric
        .sample_string(&mut rand::rng(), 6)
        .to_ascii_lowercase();
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
            eprintln!(
                "[pilot] approval window elapsed with no decision; rejecting (deny-by-default)."
            );
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
    // J11 — the same vm gate as a fresh run, over the tasks a resume can
    // still execute. Without it a resumed run was the way around fail-closed.
    let sandbox_avail = wingman_autonomous::sandbox::TierAvailability::probe(
        &cfg.pilot.sandbox,
        &wingman_autonomous::pr::SystemCommandRunner,
    );
    let pending: Vec<wingman_autonomous::Task> = state
        .tasks
        .iter()
        .filter(|t| {
            !matches!(
                t.status,
                wingman_autonomous::TaskStatus::Done | wingman_autonomous::TaskStatus::Review
            )
        })
        .cloned()
        .collect();
    if refuse_unisolated_vm_tasks(&cfg.pilot.sandbox, &sandbox_avail, &pending) {
        return Ok(ExitCode::from(2));
    }
    let base_branch = std::env::var("WINGMAN_PILOT_BASE_BRANCH")
        .unwrap_or_else(|_| cfg.pilot.pr.base_branch.clone());
    let orch_cfg = wingman_autonomous::orchestrator::OrchestratorConfig {
        max_concurrent_agents: cfg.pilot.max_concurrent_agents,
        task_timeout: std::time::Duration::from_secs(cfg.pilot.task_timeout_secs),
        project_root: project.root.clone(),
        run_id: run_id.clone(),
        base_commit: state.base_commit.clone(),
        use_real_worktrees: true,
        max_usd: cfg.pilot.max_usd,
        max_total_tokens: cfg.pilot.max_total_tokens,
        max_retries_per_task: cfg.pilot.max_retries_per_task,
        desktop_inbox: wingman_autonomous::notify::desktop_target(
            wingman_autonomous::notify::NotificationSeverity::Escalation,
            &cfg.pilot.notifications,
        ),
        sample_host_load: capability_on(&cfg.pilot, "adaptive_concurrency"),
        speculative_prespawn: capability_on(&cfg.pilot, "speculative_prespawn"),
        warm_cmd: cfg.pilot.turn_gate_cmd.clone(),
    };
    let stats_path = wingman_config::global_dir()
        .ok()
        .map(|g| g.join("stats.jsonl"));
    let routing = load_routing_aggregates(stats_path.as_deref());
    let inputs = wingman_autonomous::pipeline::PipelineInputs {
        provider: provider.clone(),
        manager_model: selection.model.clone(),
        worker_spawner: build_real_worker_spawner(
            cfg.pilot
                .worker_model
                .as_deref()
                .unwrap_or(&selection.model),
            &selection.model,
            routing,
            learned_routing(&cfg, &project.root),
            std::time::Duration::from_secs(cfg.pilot.task_timeout_secs),
            turn_rollback_after(&cfg.pilot),
            capability_on(&cfg.pilot, "checkpoint_hygiene"),
            cfg.pilot.sandbox.clone(),
            sandbox_avail.clone(),
            tool_synthesis_for(&cfg.pilot, &project.config_file),
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
        disabled_tools: cfg.tools.disabled_tools.clone(),
        run_reviewer: capability_on(&cfg.pilot, "per_task_reviewer"),
        critic: critic(
            &cfg,
            &cfg.pilot,
            cfg.pilot
                .worker_model
                .as_deref()
                .unwrap_or(&selection.model),
            wingman_autonomous::pipeline::AuxAgent {
                provider: provider.clone(),
                model: selection.model.clone(),
            },
        )?,
        // See the run() path: strip the provider prefix so the reviewer model
        // is a bare id the provider's API accepts.
        reviewer_model: match cfg
            .pilot
            .reviewer_model
            .clone()
            .or_else(|| cfg.pilot.default_model.clone())
        {
            Some(s) => runtime::resolve_selection(&cfg, Some(&s))
                .map(|sel| sel.model)
                .unwrap_or_else(|_| selection.model.clone()),
            None => selection.model.clone(),
        },
        sandbox_default_tier: cfg.pilot.sandbox.default_tier.clone(),
        sandbox_availability: sandbox_avail,
        dangerous_paths: cfg.pilot.approval.dangerous_paths.clone(),
        merge_fixer: capability_on(&cfg.pilot, "merge_fixer"),
        knowledge_keeper: knowledge_keeper(
            &cfg,
            &cfg.pilot,
            wingman_autonomous::pipeline::AuxAgent {
                provider,
                model: selection.model.clone(),
            },
        ),
    };

    eprintln!("[pilot] resume: driving manager loop for run {run_id}");
    let outcome = wingman_autonomous::pipeline::run_to_completion(store, inputs)
        .await
        .context("pipeline run_to_completion")?;
    for trigger in &outcome.escalation_triggers {
        eprintln!(
            "[pilot] resume: escalation: {} — {}",
            trigger.short_label(),
            trigger.render()
        );
    }
    if !outcome.failed_tasks.is_empty() || outcome.escalation_packet.is_some() {
        if !outcome.failed_tasks.is_empty() {
            eprintln!(
                "[pilot] resume: tasks ended in non-Done state: {:?}",
                outcome.failed_tasks
            );
        }
        if let Some(packet) = &outcome.escalation_packet {
            eprintln!(
                "[pilot] resume: escalation packet written: {}",
                packet.display()
            );
        }
        return Ok(ExitCode::from(2));
    }
    if let Some(pr) = outcome.pr {
        eprintln!("[pilot] resume: PR URL → {}", pr.url);
    }
    Ok(ExitCode::SUCCESS)
}

/// J11 — refuse to start when a task needs vm-tier isolation this machine
/// cannot provide, rather than run migrations / infra / irreversible work
/// with weaker isolation. `[pilot.sandbox].allow_unsandboxed_vm_tasks` opts
/// out. Returns true (after explaining) when the run must not start.
fn refuse_unisolated_vm_tasks(
    sandbox: &wingman_config::PilotSandboxConfig,
    avail: &wingman_autonomous::sandbox::TierAvailability,
    tasks: &[wingman_autonomous::Task],
) -> bool {
    use wingman_autonomous::sandbox::{select_tier, IsolationTier};
    let Err(reason) = &avail.vm else {
        return false;
    };
    if sandbox.allow_unsandboxed_vm_tasks {
        return false;
    }
    let default_tier = IsolationTier::parse(&sandbox.default_tier);
    let vm_tasks: Vec<&str> = tasks
        .iter()
        .filter(|t| select_tier(t, default_tier) == IsolationTier::Vm)
        .map(|t| t.id.as_str())
        .collect();
    if vm_tasks.is_empty() {
        return false;
    }
    eprintln!(
        "[pilot] refusing to run: {} task(s) need vm-tier isolation \
         (migrations / infra / irreversible / untrusted) but the vm tier is \
         unavailable here ({reason}):",
        vm_tasks.len()
    );
    for id in &vm_tasks {
        eprintln!("[pilot]   - {id}");
    }
    eprintln!(
        "[pilot] relabel/split the task, configure [pilot.sandbox.vm], or set \
         [pilot.sandbox].allow_unsandboxed_vm_tasks = true to accept weaker isolation."
    );
    true
}

/// J11 — the sandbox one task's worker runs in, or `None` for the host.
///
/// `Err` for a vm-tier task this machine cannot isolate, unless
/// `allow_unsandboxed_vm_tasks`. The start-of-run gate
/// ([`refuse_unisolated_vm_tasks`]) only sees the plan; this catches a task
/// the manager adds (`add_task`) or splits off mid-run, which would otherwise
/// quietly degrade to weaker isolation.
fn worker_sandbox_for(
    task: &wingman_autonomous::Task,
    sandbox: &wingman_config::PilotSandboxConfig,
    avail: &wingman_autonomous::sandbox::TierAvailability,
) -> std::result::Result<Option<wingman_autonomous::sandbox::WorkerSandbox>, String> {
    use wingman_autonomous::sandbox::{resolve_effective_tier, select_tier, IsolationTier};
    let requested = select_tier(task, IsolationTier::parse(&sandbox.default_tier));
    if let (IsolationTier::Vm, Err(why)) = (requested, &avail.vm) {
        if !sandbox.allow_unsandboxed_vm_tasks {
            return Err(format!(
                "task {} needs vm-tier isolation, which is unavailable here ({why});                  refusing to run it with weaker isolation",
                task.id
            ));
        }
    }
    let (tier, degraded) = resolve_effective_tier(requested, avail);
    if degraded {
        tracing::warn!(
            target: "pilot::sandbox",
            task = %task.id,
            "task wants the {} tier but runs in {}: no backend for it here",
            requested.as_str(),
            tier.as_str()
        );
    }
    Ok(
        (tier != IsolationTier::Host).then(|| wingman_autonomous::sandbox::WorkerSandbox {
            tier,
            config: sandbox.clone(),
            global_config: wingman_config::global_config_path().ok(),
        }),
    )
}

/// The config learned routing reads (`[router].learned_min_samples`, and the
/// providers a pick must still resolve to) paired with the repo key routing
/// rows are recorded under, or `None` when learned routing is off.
fn learned_routing(
    cfg: &Config,
    project_root: &std::path::Path,
) -> Option<std::sync::Arc<(Config, String)>> {
    cfg.router
        .learned_min_samples
        .map(|_| std::sync::Arc::new((cfg.clone(), project_root.to_string_lossy().to_string())))
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
///
/// `learned` is the config with `[router].learned_min_samples` set and the
/// repo it reads `learn.db` for. When set, and a model has won the task's role
/// there, that model takes the base attempt ahead of the E6 choice.
///
/// `sandbox` + `avail` pick each task's J11 tier: a container/vm task runs
/// its worker in that sandbox, degraded to what this machine can honour.
///
/// `tool_synthesis` is [`tool_synthesis_for`]'s answer, handed to every
/// worker.
#[allow(clippy::too_many_arguments)]
fn build_real_worker_spawner(
    worker_model: &str,
    manager_model: &str,
    routing: Option<std::sync::Arc<wingman_autonomous::learning::Aggregates>>,
    learned: Option<std::sync::Arc<(Config, String)>>,
    task_timeout: std::time::Duration,
    turn_rollback_after: u32,
    checkpoint_hygiene: bool,
    sandbox: wingman_config::PilotSandboxConfig,
    avail: wingman_autonomous::sandbox::TierAvailability,
    tool_synthesis: Option<wingman_autonomous::approval::ApprovalTier>,
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
            let worker_sandbox = worker_sandbox_for(&ctx.task, &sandbox, &avail);
            let learned = learned.clone();
            Box::pin(async move {
                // E5 rung 2: escalate to the manager model when the
                // orchestrator flagged this attempt as needing it. Otherwise
                // learned routing, then E6 adaptive routing, picks the base
                // model per role.
                let learned_pick = match (&learned, ctx.escalate_model) {
                    (Some(l), false) => runtime::learned_model(&l.0, ctx.task.role.as_str(), &l.1),
                    _ => None,
                };
                let model = if ctx.escalate_model {
                    Some(manager_model)
                } else if learned_pick.is_some() {
                    learned_pick
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
                let mut spec = wingman_autonomous::worker::WorkerSpec {
                    wingman_bin,
                    role: task.role.clone(),
                    task,
                    worktree: ctx.worktree.clone(),
                    session_id: ctx.session_id.clone(),
                    model,
                    timeout: task_timeout,
                    cmd_rx,
                    rung: ctx.rung,
                    turn_rollback_after,
                    checkpoint_hygiene,
                    sandbox: None,
                    tool_synthesis,
                };
                // Recorded on the task (with its ladder attempt), so the
                // manager and `pilot status` see why it failed rather than a
                // bare Failed.
                match worker_sandbox {
                    Ok(sb) => spec.sandbox = sb,
                    Err(why) => {
                        wingman_autonomous::worker::record_failure(
                            &ctx.store,
                            &spec,
                            &ctx.agent_id,
                            why.clone(),
                        )
                        .await;
                        return Err(wingman_autonomous::orchestrator::OrchestratorError::Spawn(
                            why,
                        ));
                    }
                }
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

/// `wingman pilot export` — a run as a pull-request description.
///
/// The body the orchestrator opens a PR with (goal, tasks, run cost) plus what
/// only the workers' own transcripts know: which files each changed and by how
/// much, whether its verification gate passed, and its tokens. Everything
/// passes through the secret redactor, since the output exists to be pasted
/// into a PR.
pub async fn export(run_id: Option<String>, json: bool) -> Result<ExitCode> {
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let pick = pick_run(run_id)?;
    let state = wingman_autonomous::dashboard::load_state(&pick.dir)?;
    let workers = worker_exports(&state, &project.sessions_dir);
    // The title is the goal's first line, already counted in the body's total.
    let (title, _) = wingman_core::redact::redact_output_secrets(
        &wingman_autonomous::pr::render_pr_title(&state),
    );
    let (body, redacted) = render_run_export(&state, &workers);
    if json {
        let workers: Vec<_> = workers
            .iter()
            .map(|w| {
                serde_json::json!({
                    "agent": w.agent, "task": w.task, "session": w.session,
                })
            })
            .collect();
        let out = serde_json::json!({
            "run_id": state.run_id,
            "status": state.status,
            "pr_url": state.pr_url,
            "title": title,
            "body": body,
            "workers": workers,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("# {title}\n\n{body}");
    }
    if redacted > 0 {
        eprintln!("[pilot] redacted {redacted} secret(s)");
    }
    Ok(ExitCode::SUCCESS)
}

/// One worker's transcript, reduced.
struct WorkerExport {
    /// The worker's display name, or its id for runs that predate names.
    agent: String,
    task: Option<String>,
    session: wingman_session::export::SessionExport,
}

/// Every worker whose transcript is still on disk, in spawn order. A worker
/// with no session id or a deleted log is left out rather than failing the
/// export: the run's own summary is still worth having.
fn worker_exports(
    state: &wingman_autonomous::RunState,
    sessions_dir: &std::path::Path,
) -> Vec<WorkerExport> {
    state
        .agents
        .iter()
        .filter_map(|a| {
            let path = wingman_session::session_path(sessions_dir, a.session_id.as_deref()?)?;
            let session = wingman_session::export::export_file(&path).ok()?;
            let task = state
                .tasks
                .iter()
                .find(|t| t.agent.as_deref() == Some(&a.id))
                .map(|t| t.id.clone())
                .or_else(|| a.current_task.clone());
            Some(WorkerExport {
                agent: if a.name.is_empty() {
                    a.id.clone()
                } else {
                    a.name.clone()
                },
                task,
                session,
            })
        })
        .collect()
}

/// The PR body with a worker-sessions section ahead of its footer, and how
/// many secrets were redacted from it in total.
fn render_run_export(
    state: &wingman_autonomous::RunState,
    workers: &[WorkerExport],
) -> (String, usize) {
    use std::collections::BTreeMap;
    use std::fmt::Write as _;

    let cell = |s: &str| s.replace('|', "\\|").replace(['\n', '\r'], " ");
    let mut section = String::new();
    if !workers.is_empty() {
        let _ = writeln!(
            section,
            "## Worker sessions\n\n\
             | Worker | Task | Files | Verification | Tokens | Cost |\n\
             |---|---|---|---|---:|---:|"
        );
        let mut files: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
        for w in workers {
            let x = &w.session;
            let _ = writeln!(
                section,
                "| {} | {} | {} | {} | {} | {} |",
                cell(&w.agent),
                cell(w.task.as_deref().unwrap_or("-")),
                x.files_line(),
                x.receipts_line(),
                x.total_tokens,
                x.usd.map_or("unpriced".into(), |u| format!("${u:.4}")),
            );
            for f in &x.files {
                let e = files.entry(&f.path).or_default();
                e.0 += f.added;
                e.1 += f.removed;
            }
        }
        if !files.is_empty() {
            let _ = writeln!(
                section,
                "\n## Files changed\n\n| File | + | − |\n|---|---:|---:|"
            );
            for (path, (added, removed)) in files {
                let _ = writeln!(section, "| `{}` | {added} | {removed} |", cell(path));
            }
        }
        section.push('\n');
    }

    let body = wingman_autonomous::pr::render_pr_body(state);
    // `render_pr_body` ends with its "Opened by wingman pilot" footer.
    let at = body.rfind("_Opened by wingman pilot").unwrap_or(body.len());
    let (body, mut redacted) = wingman_core::redact::redact_output_secrets(&format!(
        "{}{section}{}",
        &body[..at],
        &body[at..]
    ));
    redacted += workers.iter().map(|w| w.session.redacted).sum::<usize>();
    (body, redacted)
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
///
/// J13 — `watch` keeps the poll but also wakes on debounced file changes
/// (local sources only) and on `pilot hooks install` git hooks (every
/// source); see `wingman_autonomous::watcher`. Event-woken cycles count
/// towards `cycles` and go through the same queue, trust and cap.
pub async fn daemon(cfg: Config, cycles: usize, dry_run: bool, watch: bool) -> Result<ExitCode> {
    use std::time::Duration;
    use wingman_autonomous::watcher::{self, Wake};

    let pilot = &cfg.pilot;
    if !pilot.daemon.enabled && cycles == 0 {
        eprintln!(
            "[pilot] daemon is disabled. Set `[pilot.daemon].enabled = true` in config, \
             or pass `--cycles N` for a one-shot discovery pass."
        );
        return Ok(ExitCode::from(1));
    }

    // Honesty check: warn (don't fail) for any configured source we can't
    // actually poll, so the daemon doesn't look broken when it silently
    // finds nothing.
    const IMPLEMENTED_SOURCES: &[&str] = &[
        "github_issues",
        "todos",
        "ci_failures",
        "dependabot",
        "coverage_gaps",
        "intake",
        "ask",
        "pr_reviews",
    ];
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

    let mut watcher = if watch {
        let intake = pilot
            .daemon
            .sources
            .iter()
            .any(|s| s == "intake")
            .then(|| project.root.join(&pilot.daemon.intake_dir));
        let debounce = Duration::from_millis(pilot.daemon.watch_debounce_ms);
        Some(
            watcher::Watcher::start(&project.root, intake, debounce)
                .map_err(|e| anyhow::anyhow!("pilot daemon --watch: {e}"))?,
        )
    } else {
        None
    };

    eprintln!(
        "[pilot] daemon starting (sources: {:?}, auto_threshold: {:.2}, interval: {}s, \
         feedback: {}{}){}",
        pilot.daemon.sources,
        pilot.daemon.auto_threshold,
        pilot.daemon.poll_interval_secs,
        match pilot.daemon.feedback_poll_secs {
            0 => "off".to_string(),
            s => format!("every {s}s"),
        },
        if watch {
            ", watching files and git hooks"
        } else {
            ""
        },
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
    let mut last_feedback: Option<std::time::Instant> = None;

    let mut n = 0usize;
    // The first cycle, and every poll, asks every source.
    let mut wake = Wake::Poll;
    let mut next_poll = tokio::time::Instant::now() + interval;
    loop {
        if wake == Wake::Poll {
            next_poll = tokio::time::Instant::now() + interval;
        } else {
            eprintln!("[pilot] daemon cycle {n}: woken by {wake:?}");
        }
        // R2 — the post-merge feedback pass rides the discovery loop on its
        // own, slower cadence. It only reads PR state and appends to run logs
        // this repo already holds, so `--dry-run` runs it too.
        let now = std::time::Instant::now();
        if feedback_due(last_feedback, now, pilot.daemon.feedback_poll_secs) {
            last_feedback = Some(now);
            poll_feedback(&runner, &project.root, n).await;
        }

        let results = wingman_autonomous::daemon::run_cycle(
            &runner,
            &project.root,
            &watcher::cycle_config(&pilot.daemon, wake),
            propose_floor,
        );
        if results.is_empty() {
            eprintln!("[pilot] daemon cycle {n}: no candidates");
        }
        // Per-cycle, not per-daemon: the cap bounds one burst of discovery,
        // and `poll_interval_secs` governs the rate across cycles.
        let mut dispatched = 0usize;
        let mut deferred = 0usize;
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
            // A dry run leaves no durable trace: the queue is what later
            // daemons dedup on, so recording a candidate here would stop the
            // real daemon from ever dispatching what the dry run only showed.
            let queued = if dry_run {
                Ok(())
            } else {
                append_daemon_queue(&queue_path, cand, *action)
            };
            if let Err(e) = queued {
                eprintln!("[pilot] daemon: failed to queue candidate: {e:#}");
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
                // Bound how much one cycle may start on its own. The candidate
                // is already queued above, so hitting the cap defers work
                // rather than dropping it — and it is said out loud, because a
                // cap nobody is told about reads as "there was nothing else".
                let cap = pilot.daemon.max_auto_dispatch_per_cycle;
                if cap != 0 && dispatched >= cap {
                    deferred += 1;
                    eprintln!(
                        "[pilot] daemon: cap reached ({cap}/cycle) — {:?} stays queued",
                        cand.title
                    );
                    continue;
                }
                if dry_run {
                    // Validation path (#34): show the decision, open nothing.
                    eprintln!(
                        "[pilot] daemon: [dry-run] would auto-dispatch a run for {:?} \
                         (source {}, score {:.2})",
                        cand.title,
                        cand.source,
                        cand.score()
                    );
                    continue;
                }
                if let Some(run_id) =
                    wingman_autonomous::pr_reviews::run_id_from_source(&cand.source)
                {
                    dispatched += 1;
                    rework_pr_reviews(&cfg, &runner, &project.root, run_id).await;
                    continue;
                }
                eprintln!("[pilot] daemon: auto-dispatching run for {:?}", cand.title);
                dispatched += 1;
                let opts = PilotOptions {
                    goal: cand.title.clone(),
                    yes: true, // trusted, already scored above threshold
                    ..PilotOptions::default()
                };
                match run(cfg.clone(), opts).await {
                    Ok(code) => {
                        eprintln!("[pilot] daemon: dispatched run exited {code:?}")
                    }
                    // `{e:#}`, not `{e}`. These are anyhow errors carrying a
                    // context chain, and the bare form prints only the outermost
                    // link — so a real failure surfaced as
                    // "dispatched run failed: pipeline run_to_completion",
                    // which names the step that failed and nothing about why.
                    // The daemon runs unattended; its log is the only account
                    // anyone gets.
                    Err(e) => eprintln!("[pilot] daemon: dispatched run failed: {e:#}"),
                }
            }
        }

        if deferred > 0 {
            eprintln!(
                "[pilot] daemon cycle {n}: dispatched {dispatched}, deferred {deferred} to a \
                 later cycle ([pilot.daemon].max_auto_dispatch_per_cycle)"
            );
        }

        n += 1;
        if cycles != 0 && n >= cycles {
            eprintln!("[pilot] daemon: completed {n} cycle(s), exiting.");
            return Ok(ExitCode::SUCCESS);
        }
        wake = match watcher.as_mut() {
            // A file change only runs the local sources. With none of them
            // configured its cycle would ask nothing, yet still log and count
            // towards `--cycles`, so keep waiting instead.
            Some(w) => loop {
                let wake = w.wait(&runner, next_poll).await;
                if wake != Wake::FileChange
                    || !watcher::cycle_config(&pilot.daemon, wake)
                        .sources
                        .is_empty()
                {
                    break wake;
                }
            },
            None => {
                tokio::time::sleep(interval).await;
                Wake::Poll
            }
        };
    }
}

/// J13 — install the git hooks that wake `pilot daemon --watch`.
pub async fn hooks_install() -> Result<ExitCode> {
    let root = ProjectPaths::discover(&std::env::current_dir()?).root;
    let exe = std::env::current_exe().context("locating the wingman binary")?;
    let result = wingman_autonomous::watcher::install_hooks(
        &wingman_autonomous::pr::SystemCommandRunner,
        &root,
        &exe,
    )
    .map_err(|e| anyhow::anyhow!("pilot hooks install: {e}"))?;
    for path in &result.installed {
        println!("installed {}", path.display());
    }
    for path in &result.skipped {
        eprintln!(
            "[pilot] hooks: skipped {}: a hook wingman did not write is already there",
            path.display()
        );
    }
    // Git for Windows ignores the directory in a `#!` line and looks the
    // binary's file name up on PATH; say so now rather than let every hook
    // fail silently later.
    if cfg!(windows) {
        let name = exe.file_name().unwrap_or_default();
        let on_path = std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()));
        if !on_path {
            eprintln!(
                "[pilot] hooks: {} is not on PATH; Git for Windows finds the hooks' \
                 interpreter there, so they will not run until it is",
                name.to_string_lossy()
            );
        }
    }
    if !result.installed.is_empty() {
        println!("Run `wingman pilot daemon --watch` to react to them.");
    }
    Ok(ExitCode::SUCCESS)
}

/// J13 — remove the git hooks `hooks_install` wrote.
pub async fn hooks_uninstall() -> Result<ExitCode> {
    let root = ProjectPaths::discover(&std::env::current_dir()?).root;
    let removed = wingman_autonomous::watcher::uninstall_hooks(
        &wingman_autonomous::pr::SystemCommandRunner,
        &root,
    )
    .map_err(|e| anyhow::anyhow!("pilot hooks uninstall: {e}"))?;
    if removed.is_empty() {
        eprintln!("[pilot] hooks: no wingman hooks installed in this repo");
    }
    for path in &removed {
        println!("removed {}", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

/// J13 — the body of an installed git hook: record that `hook` fired for a
/// watching daemon. Always exits 0; git ignores a post-hook's status anyway,
/// and a failure here must never look like a failed commit.
pub fn record_hook(hook: &str) -> ExitCode {
    let recorded = std::env::current_dir()
        .and_then(|cwd| wingman_autonomous::watcher::record_hook_event(&cwd, hook));
    if let Err(e) = recorded {
        eprintln!("wingman: {hook} hook: {e}");
    }
    ExitCode::SUCCESS
}

/// `pr_reviews` dispatch: rework the trusted review threads on one of pilot's
/// PRs as a nested run stacked on the PR's own branch (no new PR), then push,
/// reply on the threads and record the round via `pr_reviews::finish_round`.
async fn rework_pr_reviews(
    cfg: &Config,
    runner: &dyn wingman_autonomous::pr::CommandRunner,
    root: &std::path::Path,
    run_id: &str,
) {
    use wingman_autonomous::pr_reviews;

    let pilot = &cfg.pilot;
    // Re-read live rather than trusting discovery: a reviewer may have
    // resolved the threads, or the PR merged, since the cycle started.
    let target = match pr_reviews::load_target(
        runner,
        root,
        &run_dir(root, run_id),
        &pilot.daemon.trusted_authors,
        pilot.daemon.max_review_rounds,
    ) {
        Ok(Some(t)) => t,
        Ok(None) => {
            eprintln!("[pilot] daemon: run {run_id}'s PR has no review threads left to address");
            return;
        }
        Err(e) => {
            eprintln!("[pilot] daemon: pr_reviews for run {run_id}: {e}");
            return;
        }
    };
    let Some(budget) = pr_reviews::round_budget(pilot.max_usd, target.spent_usd) else {
        eprintln!(
            "[pilot] daemon: {} has spent its review budget (${:.2} of [pilot].max_usd ${:.2}) \
             — leaving its threads to a person",
            target.pr_url, target.spent_usd, pilot.max_usd
        );
        return;
    };
    if let Err(e) = pr_reviews::fetch_head(runner, root, &target) {
        eprintln!("[pilot] daemon: fetching {}: {e}", target.branch);
        return;
    }

    let rework_id = new_run_id();
    eprintln!(
        "[pilot] daemon: review round {} on {} ({} thread(s)) as run {rework_id}",
        target.rounds + 1,
        target.pr_url,
        target.threads.len()
    );
    let opts = PilotOptions {
        goal: pr_reviews::rework_goal(&target),
        yes: true, // trusted reviewers only, already scored above threshold
        no_pr: true,
        base: Some(target.head_sha.clone()),
        max_usd: Some(budget),
        run_id: Some(rework_id.clone()),
        rework_branch: Some(target.branch.clone()),
        ..PilotOptions::default()
    };
    match run(cfg.clone(), opts).await {
        Ok(code) => eprintln!("[pilot] daemon: review rework run exited {code:?}"),
        Err(e) => eprintln!("[pilot] daemon: review rework run failed: {e:#}"),
    }
    match pr_reviews::finish_round(
        runner,
        root,
        &target,
        &rework_id,
        &run_dir(root, &rework_id),
    )
    .await
    {
        Ok(o) => {
            eprintln!(
                "[pilot] daemon: review round {} on {}: {} — resolved {}/{} thread(s), ${:.2}",
                o.round, target.pr_url, o.outcome, o.addressed, o.threads, o.usd
            );
            for e in &o.errors {
                eprintln!("[pilot] daemon:   {e}");
            }
        }
        Err(e) => eprintln!("[pilot] daemon: recording review round: {e}"),
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
        poll_feedback(&runner, &project.root, n).await;

        n += 1;
        if cycles != 0 && n >= cycles {
            eprintln!("[pilot] feedback: completed {n} cycle(s), exiting.");
            return Ok(ExitCode::SUCCESS);
        }
        tokio::time::sleep(interval).await;
    }
}

/// One R2 feedback pass: poll every run awaiting an outcome and record the
/// terminal ones. Returns how many outcomes it recorded. `pilot feedback` runs
/// it each cycle; `pilot daemon` on `[pilot.daemon].feedback_poll_secs`.
async fn poll_feedback(
    runner: &dyn wingman_autonomous::pr::CommandRunner,
    project_root: &std::path::Path,
    cycle: usize,
) -> usize {
    let pending = feedback_pending_runs(project_root).await;
    if pending.is_empty() {
        eprintln!("[pilot] feedback cycle {cycle}: no open PRs awaiting outcome");
    }
    let mut recorded = 0;
    for (dir, pr_url) in pending {
        let mut store = match wingman_autonomous::store::RunStore::load(&dir).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[pilot] feedback: load {} failed: {e}", dir.display());
                continue;
            }
        };
        match wingman_autonomous::feedback::poll_and_record(
            runner,
            &mut store,
            project_root,
            &pr_url,
        )
        .await
        {
            Ok(Some(kind)) => {
                recorded += 1;
                eprintln!("[pilot] feedback: {pr_url} → {kind:?}")
            }
            Ok(None) => eprintln!("[pilot] feedback: {pr_url} still open"),
            Err(e) => eprintln!("[pilot] feedback: {pr_url} poll failed: {e}"),
        }
    }
    recorded
}

/// Whether the daemon's R2 feedback pass is due: never with `every_secs == 0`,
/// on the first cycle, then once `every_secs` have passed since the last pass.
/// Checked at cycle boundaries, so a cycle busy with a dispatched run delays
/// the pass rather than running it alongside.
fn feedback_due(
    last: Option<std::time::Instant>,
    now: std::time::Instant,
    every_secs: u64,
) -> bool {
    every_secs != 0
        && last.is_none_or(|t| now.duration_since(t) >= std::time::Duration::from_secs(every_secs))
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
                    evs.iter()
                        .any(|e| matches!(e, wingman_autonomous::model::Event::PrOutcome { .. }))
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

/// J12 — install skill packs: `specs`, or `[pilot.skills].packs` when none are
/// given. With `[pilot.skills].index` set, the specs are resolved against the
/// index together with their dependencies and each pack's signature is checked;
/// without one, each `owner/name@version` is cloned from
/// `https://github.com/owner/name` (tag `v<version>`), which is unsigned and so
/// needs `allow_unsigned`. Packs land in `~/.wingman/packs/<slug>/` and their
/// role/lessons files are copied into `~/.wingman/agents/` so the role loader
/// picks them up.
pub async fn skills_install(
    cfg: Config,
    specs: Vec<String>,
    allow_unsigned: bool,
) -> Result<ExitCode> {
    use wingman_autonomous::skillpack;
    let specs = if specs.is_empty() {
        cfg.pilot.skills.packs.clone()
    } else {
        specs
    };
    let (refs, errs) = skillpack::parse_pack_list(&specs);
    for e in &errs {
        eprintln!("[pilot] skills: bad spec — {e}");
    }
    if refs.is_empty() {
        eprintln!(
            "[pilot] skills: no valid packs to install (pass specs or set [pilot.skills].packs)"
        );
        return Ok(if errs.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        });
    }
    let home = wingman_config::user_home()
        .map_err(|e| anyhow!("cannot resolve home directory for pack install: {e}"))?;
    let runner = wingman_autonomous::pr::SystemCommandRunner;
    let resolved = if cfg.pilot.skills.index.trim().is_empty() {
        refs.iter()
            .map(|r| skillpack::ResolvedPack {
                pack: r.clone(),
                source: format!("https://github.com/{}/{}", r.owner, r.name),
                signature: None,
                deps: Vec::new(),
            })
            .collect()
    } else {
        let index = skillpack::load_index(&runner, &cfg.pilot.skills.index, &home)
            .map_err(|e| anyhow!("skills: {e}"))?;
        skillpack::resolve(&index, &refs).map_err(|e| anyhow!("skills: {e}"))?
    };
    let mut failures = errs.len();
    for r in &resolved {
        match skillpack::fetch_pack(&runner, r, &home, allow_unsigned) {
            Ok(dest) => eprintln!(
                "[pilot] skills: installed {} ({}) → {}",
                r.pack,
                if r.signature.is_some() {
                    "signed"
                } else {
                    "unsigned"
                },
                dest.display()
            ),
            Err(e) => {
                eprintln!("[pilot] skills: {} failed — {e}", r.pack);
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

/// J12 — search `[pilot.skills].index` for packs whose name or description
/// contains `query`, newest version of each.
pub async fn skills_search(cfg: Config, query: String) -> Result<ExitCode> {
    use wingman_autonomous::skillpack;
    let home = wingman_config::user_home()
        .map_err(|e| anyhow!("cannot resolve home directory for the pack index: {e}"))?;
    let runner = wingman_autonomous::pr::SystemCommandRunner;
    let index = skillpack::load_index(&runner, &cfg.pilot.skills.index, &home)
        .map_err(|e| anyhow!("skills: {e}"))?;
    let hits = skillpack::search(&index, &query);
    if hits.is_empty() {
        eprintln!("[pilot] skills: no packs match `{query}`");
    }
    for (key, e) in hits {
        let signed = if e.signature.is_some() {
            "signed"
        } else {
            "unsigned"
        };
        println!("{key}@{}  [{signed}]  {}", e.version, e.description);
        if !e.deps.is_empty() {
            println!("    deps: {}", e.deps.join(", "));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// J12 — list installed packs from their install receipts.
pub async fn skills_list() -> Result<ExitCode> {
    use wingman_autonomous::skillpack;
    let home = wingman_config::user_home()
        .map_err(|e| anyhow!("cannot resolve home directory for installed packs: {e}"))?;
    let (receipts, errs) = skillpack::list_installed(&home);
    for e in &errs {
        eprintln!("[pilot] skills: unreadable receipt — {e}");
    }
    if receipts.is_empty() {
        eprintln!("[pilot] skills: no packs installed");
    }
    for r in &receipts {
        let signed = if r.signature.is_some() {
            "signed"
        } else {
            "unsigned"
        };
        println!("{}  [{signed}]  {}", r.pack, r.source);
        if !r.deps.is_empty() {
            println!("    deps: {}", r.deps.join(", "));
        }
    }
    Ok(if errs.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// J12 — re-verify installed packs (all, or those matching `specs` by
/// `owner/name` or exact `owner/name@X.Y.Z`). Non-zero exit on any failure,
/// so it can gate a CI job or a cron check.
pub async fn skills_verify(specs: Vec<String>, allow_unsigned: bool) -> Result<ExitCode> {
    use wingman_autonomous::skillpack;
    let home = wingman_config::user_home()
        .map_err(|e| anyhow!("cannot resolve home directory for installed packs: {e}"))?;
    let runner = wingman_autonomous::pr::SystemCommandRunner;
    let (receipts, errs) = skillpack::list_installed(&home);
    let mut failures = errs.len();
    for e in &errs {
        eprintln!("[pilot] skills: unreadable receipt — {e}");
    }
    let wanted = |pack: &str| {
        specs.is_empty()
            || specs
                .iter()
                .any(|s| pack == s || pack.starts_with(&format!("{s}@")))
    };
    let mut checked = 0;
    for r in receipts.iter().filter(|r| wanted(&r.pack)) {
        checked += 1;
        match skillpack::verify_installed(&runner, r, &home, allow_unsigned) {
            Ok(()) => eprintln!("[pilot] skills: {} ok", r.pack),
            Err(e) => {
                eprintln!("[pilot] skills: {} FAILED — {e}", r.pack);
                failures += 1;
            }
        }
    }
    if checked == 0 {
        eprintln!("[pilot] skills: no matching installed packs");
        if !specs.is_empty() {
            failures += 1;
        }
    }
    Ok(if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// J7 — the project whose `.wingman/tools/` the `pilot tools` commands act
/// on: the owning project, so running them from inside a worktree still
/// reaches the directory workers write to.
fn synthesized_tools_project() -> Result<std::path::PathBuf> {
    Ok(wingman_config::find_owning_project_root(
        &std::env::current_dir()?,
    ))
}

/// J7 — list proposed tools, approved or pending.
pub async fn tools_list() -> Result<ExitCode> {
    let tools = wingman_config::synthesized_tools(&synthesized_tools_project()?);
    if tools.is_empty() {
        eprintln!("[pilot] tools: no proposed tools in this project");
    }
    for t in &tools {
        let state = if wingman_config::trust::is_trusted(&t.path) {
            "approved"
        } else {
            "pending"
        };
        println!("{}  [{state}]  {}", t.tool.name, t.tool.description);
        println!("    command: {}", t.tool.command);
    }
    Ok(ExitCode::SUCCESS)
}

/// J7 — approve a proposed tool. Prints the command being approved, since
/// that is what every later worker and session here will be able to run.
pub async fn tools_approve(name: String) -> Result<ExitCode> {
    let Some(t) = find_synthesized_tool(&name)? else {
        return Ok(ExitCode::from(1));
    };
    let hash = wingman_config::trust::trust(&t.path)?;
    println!("Approved `{}` ({})", t.tool.name, t.path.display());
    println!("  command: {}", t.tool.command);
    println!("  sha256:  {hash}");
    println!("Editing the file revokes this; re-run `wingman pilot tools approve {name}`.");
    Ok(ExitCode::SUCCESS)
}

/// J7 — reject a proposed or approved tool: delete the file and forget it.
pub async fn tools_reject(name: String) -> Result<ExitCode> {
    let Some(t) = find_synthesized_tool(&name)? else {
        return Ok(ExitCode::from(1));
    };
    wingman_config::trust::untrust(&t.path)?;
    std::fs::remove_file(&t.path).with_context(|| format!("removing {}", t.path.display()))?;
    println!("Rejected `{}`", t.tool.name);
    Ok(ExitCode::SUCCESS)
}

fn find_synthesized_tool(name: &str) -> Result<Option<wingman_config::SynthesizedTool>> {
    let found = wingman_config::synthesized_tools(&synthesized_tools_project()?)
        .into_iter()
        .find(|t| t.tool.name == name);
    if found.is_none() {
        eprintln!(
            "[pilot] tools: no proposed tool named `{name}` (see `wingman pilot tools list`)"
        );
    }
    Ok(found)
}

/// J12 — for pack authors: the payload `ssh-keygen -Y sign -n
/// wingman-skillpack` signs for `dir` published as `spec` with `deps`. The
/// resulting `.sig` text goes in the index entry's `signature`.
pub async fn skills_digest(
    spec: String,
    dir: std::path::PathBuf,
    deps: Vec<String>,
    out: Option<std::path::PathBuf>,
) -> Result<ExitCode> {
    use wingman_autonomous::skillpack;
    let pack = skillpack::parse_pack_ref(&spec).map_err(|e| anyhow!("skills: {e}"))?;
    let deps = deps
        .iter()
        .map(|d| skillpack::parse_pack_ref(d))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("skills: {e}"))?;
    let digest = skillpack::pack_digest(&dir).map_err(|e| anyhow!("skills: {e}"))?;
    let payload = skillpack::signed_payload(&pack, &digest, &deps);
    match out {
        // Written as exact bytes: a shell redirect can re-encode or append a
        // newline (PowerShell does both), which would never verify.
        Some(path) => {
            std::fs::write(&path, &payload)?;
            eprintln!(
                "[pilot] skills: wrote payload to {}; sign it with \
                 `ssh-keygen -Y sign -f <key> -n {} {}`",
                path.display(),
                skillpack::SIGNATURE_NAMESPACE,
                path.display()
            );
        }
        None => print!("{payload}"),
    }
    Ok(ExitCode::SUCCESS)
}

/// R4 — eval / regression harness + CI gate.
///
/// Two modes:
/// - `--goals <FILE>` runs each goal live through the pilot pipeline,
///   harvesting success/usd/wall and a quality score, and writes
///   `<eval>/results.jsonl`.
/// - otherwise reads an existing `<eval>/results.jsonl` (produced earlier or
///   hand-authored).
///
/// Then it summarizes, compares to the baseline (`--baseline`, default
/// `<eval>/baseline.json`), prints the markdown report, and **exits non-zero
/// on regression** — that exit code is the CI gate. `--update-baseline`
/// rewrites the baseline from the current results and skips gating.
pub async fn eval(
    cfg: Config,
    goals_file: Option<std::path::PathBuf>,
    baseline: Option<std::path::PathBuf>,
    threshold: f64,
    update_baseline: bool,
) -> Result<ExitCode> {
    use wingman_autonomous::eval::EvalResult;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let eval_dir = project.root.join(".wingman").join("eval");
    let results_path = eval_dir.join("results.jsonl");
    let baseline_path = baseline.unwrap_or_else(|| eval_dir.join("baseline.json"));

    // Gather this run's results: live if --goals given, else from disk.
    let results: Vec<EvalResult> = if let Some(gf) = goals_file {
        let text = std::fs::read_to_string(&gf)
            .with_context(|| format!("reading goals file {}", gf.display()))?;
        let goals = wingman_autonomous::eval::parse_goals(&text)
            .map_err(|e| anyhow!("goals file {}: {e}", gf.display()))?;
        if goals.is_empty() {
            eprintln!("[pilot] eval: no goals in {}", gf.display());
            return Ok(ExitCode::from(1));
        }
        // Golden diff paths are relative to the goals file.
        let goals_dir = gf.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        eprintln!("[pilot] eval: running {} canned goal(s) live…", goals.len());
        let res = run_eval_goals(&cfg, &project.root, &goals, &goals_dir).await;
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
    // Quality from the judge and from the success proxy average together, so
    // say how much of each side was judged.
    let judged = |rs: &[eval::EvalResult]| {
        format!("{}/{}", rs.iter().filter(|r| r.judged).count(), rs.len())
    };
    let cur = eval::summarize(current);
    match baseline {
        None => (
            format!(
                "# Eval report\n\nNo baseline to compare against. {} result(s), \
                 {:.0}% success, avg ${:.2}.\n\nQuality judged against a golden reference \
                 for {} goal(s); the rest are success-proxied.\n\nRun `wingman pilot eval \
                 --update-baseline` to set one.\n",
                cur.n,
                cur.success_rate * 100.0,
                cur.avg_usd,
                judged(current)
            ),
            false,
        ),
        Some(base) => {
            let b = eval::summarize(base);
            let report = eval::compare(&cur, &b, threshold);
            (
                format!(
                    "{}\nQuality judged against a golden reference for {} goal(s) \
                     (baseline: {}); the rest are success-proxied.\n",
                    eval::render_report(&report),
                    judged(current),
                    judged(base)
                ),
                report.regressed,
            )
        }
    }
}

/// R4 — the eval judge. It runs on the `judge` task class (`[router.classes]`)
/// when that is routed, else on the planner model `pilot run` uses
/// (`[pilot].default_model`, then `default_model`). `None` when neither can be
/// built; goals with a golden reference then fall back to the success proxy.
fn eval_judge(cfg: &Config) -> Option<wingman_autonomous::pipeline::AuxAgent> {
    use wingman_autonomous::pipeline::AuxAgent;
    let routed = cfg
        .router
        .resolve_class("judge")
        .and_then(|spec| cfg.resolve_model_spec(&spec))
        .and_then(
            |(provider_id, model)| match runtime::build_provider(cfg, &provider_id) {
                Ok(provider) => Some(AuxAgent { provider, model }),
                Err(e) => {
                    eprintln!(
                        "[pilot] eval: cannot build provider {provider_id} for the judge class \
                         ({e}); using the planner model"
                    );
                    None
                }
            },
        );
    routed.or_else(|| {
        let spec = cfg
            .pilot
            .default_model
            .clone()
            .or_else(|| cfg.default_model.clone());
        let built = runtime::resolve_selection(cfg, spec.as_deref()).and_then(|sel| {
            runtime::build_provider(cfg, &sel.provider_id).map(|provider| AuxAgent {
                provider,
                model: sel.model,
            })
        });
        match built {
            Ok(judge) => Some(judge),
            Err(e) => {
                eprintln!("[pilot] eval: no judge model ({e:#}); golden goals are success-proxied");
                None
            }
        }
    })
}

/// Run each canned goal live through the pilot pipeline, harvesting metrics
/// from the resulting run state. success = the run reached Done; usd from the
/// run's recorded totals; wall from the wall clock around the call; quality
/// from [`wingman_autonomous::eval::score_quality`] — the judge's grade of the
/// run's diff against the goal's golden reference, else the success proxy.
///
/// A goal runs from its `base` (a golden commit's parent by default) and
/// never opens a PR: an eval's attempts are measurements, not contributions.
///
/// A run leaves the checkout on its integration branch, built from that
/// goal's base, where the goals file's golden diffs, the next goal and the
/// baseline read after the suite may not exist. So after each run the
/// checkout is put back where the suite found it, before the run is scored.
async fn run_eval_goals(
    cfg: &Config,
    project_root: &std::path::Path,
    goals: &[wingman_autonomous::eval::EvalGoal],
    goals_dir: &std::path::Path,
) -> Vec<wingman_autonomous::eval::EvalResult> {
    use std::time::Instant;
    use wingman_autonomous::eval::EvalResult;
    let has_golden = goals
        .iter()
        .any(|g| g.golden_commit.is_some() || g.golden_diff.is_some());
    let judge = if has_golden { eval_judge(cfg) } else { None };
    let runner = wingman_autonomous::pr::SystemCommandRunner;
    let home = current_checkout(&runner, project_root);
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
            goal: goal.goal.clone(),
            yes: true,
            no_pr: true,
            base: goal.base(),
            ..PilotOptions::default()
        };
        if let Err(e) = run(cfg.clone(), opts).await {
            eprintln!("[pilot] eval: {:?} run failed: {e:#}", goal.goal);
        }
        let wall_min = started.elapsed().as_secs_f64() / 60.0;
        if let Some(home) = &home {
            if let Err(e) = restore_checkout(&runner, project_root, home) {
                eprintln!("[pilot] eval: could not return the checkout to {home}: {e}");
            }
        }

        // Find the run this goal produced (newest id not seen before) and
        // read its terminal status + spend.
        let state = wingman_autonomous::dashboard::list_runs(project_root)
            .unwrap_or_default()
            .into_iter()
            .find(|r| !before.contains(&r.run_id))
            .and_then(|r| wingman_autonomous::dashboard::load_state(&r.dir).ok());
        let success = state
            .as_ref()
            .is_some_and(|s| s.status == wingman_autonomous::RunStatus::Done);
        let usd = state.as_ref().map_or(0.0, |s| s.totals.usd);

        let judge_llm = judge.as_ref().map(|j| ProviderLlm {
            provider: j.provider.as_ref(),
            model: j.model.clone(),
            max_tokens: 1024,
        });
        let score = wingman_autonomous::eval::score_quality(
            judge_llm.as_ref().map(|l| l as &dyn PlannerLlm),
            &runner,
            project_root,
            goals_dir,
            goal,
            success,
            state
                .as_ref()
                .map(|s| (s.base_commit.as_str(), s.integration_branch.as_str())),
        )
        .await;

        out.push(EvalResult {
            goal: goal.goal.clone(),
            success,
            usd,
            wall_min,
            quality: score.quality,
            judged: score.judged,
        });
        eprintln!(
            "[pilot] eval: {:?} → {} (${usd:.2}, {wall_min:.1}m, quality {:.2} {})",
            goal.goal,
            if success { "ok" } else { "fail" },
            score.quality,
            if score.judged { "judged" } else { "proxied" }
        );
        if let Some(note) = &score.note {
            eprintln!("[pilot] eval:   {note}");
        }
    }
    out
}

/// Where HEAD is in `root`: its branch, or the commit when detached. `None`
/// outside a git repository.
fn current_checkout(
    runner: &dyn wingman_autonomous::pr::CommandRunner,
    root: &std::path::Path,
) -> Option<String> {
    let read = |args: &[&str]| {
        runner
            .run("git", args, root)
            .ok()
            .filter(|o| o.success())
            .map(|o| o.stdout.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    read(&["symbolic-ref", "--short", "-q", "HEAD"]).or_else(|| read(&["rev-parse", "HEAD"]))
}

/// Check out `checkout` (from [`current_checkout`]) in `root` again.
fn restore_checkout(
    runner: &dyn wingman_autonomous::pr::CommandRunner,
    root: &std::path::Path,
    checkout: &str,
) -> std::result::Result<(), String> {
    let out = runner
        .run("git", &["checkout", "-q", checkout], root)
        .map_err(|e| e.to_string())?;
    if out.success() {
        Ok(())
    } else {
        Err(out.stderr.trim().to_string())
    }
}

fn read_eval_results(path: &std::path::Path) -> Result<Vec<wingman_autonomous::eval::EvalResult>> {
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

/// Phase 8.4 — `pilot validate-providers`: run the canned `--version-only`
/// plan ([`wingman_autonomous::provider_matrix`]) against each configured
/// provider with credentials, one scratch repo each, under `max_usd` and
/// `max_tokens`, and write `matrix.md` + `matrix.json`.
///
/// Exit 1 when any provider failed, 2 when none could run, 0 otherwise.
pub async fn validate_providers(
    cfg: Config,
    only: Vec<String>,
    max_usd: f64,
    max_tokens: u64,
    out: Option<std::path::PathBuf>,
) -> Result<ExitCode> {
    use wingman_autonomous::provider_matrix::{self, MatrixReport, MatrixRow, Verdict};
    // A cap of 0 means "no cap" everywhere else in pilot; here it would mean
    // an unbounded bill per provider.
    if !max_usd.is_finite() || max_usd <= 0.0 || max_tokens == 0 {
        return Err(anyhow!(
            "--max-usd and --max-tokens must both be above 0: every provider run spends real money"
        ));
    }
    // Workers are real child processes; Ctrl+C must take them down too.
    crate::shutdown::install();
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let out_dir = out.unwrap_or_else(|| project.root.join(".wingman").join("provider-validation"));

    // The same vm gate a real run applies, over the canned task.
    let sandbox_avail = wingman_autonomous::sandbox::TierAvailability::probe(
        &cfg.pilot.sandbox,
        &wingman_autonomous::pr::SystemCommandRunner,
    );
    let canned: Vec<wingman_autonomous::Task> = provider_matrix::canned_plan()
        .into_iter()
        .map(|p| {
            let mut t = wingman_autonomous::Task::new(p.id, p.role, p.title);
            t.writes = p.writes;
            t.acceptance = p.acceptance;
            t.reversibility = p.reversibility;
            t
        })
        .collect();
    if refuse_unisolated_vm_tasks(&cfg.pilot.sandbox, &sandbox_avail, &canned) {
        return Ok(ExitCode::from(2));
    }

    let ids = if only.is_empty() {
        cfg.providers.keys().cloned().collect()
    } else {
        only
    };
    let env = |k: &str| std::env::var(k).ok();
    let mut rows = Vec::with_capacity(ids.len());
    for id in ids {
        let model = match validation_target(&cfg, &id, &env) {
            Ok(model) => model,
            Err((model, reason)) => {
                eprintln!("[pilot] validate: {id}: skipped ({reason})");
                rows.push(MatrixRow::skipped(&id, model, reason));
                continue;
            }
        };
        let provider = match runtime::build_provider(&cfg, &id) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[pilot] validate: {id}: skipped ({e})");
                rows.push(MatrixRow::skipped(&id, Some(model), e.to_string()));
                continue;
            }
        };
        eprintln!("[pilot] validate: {id}/{model}: running the canned plan…");
        // `provider/model`, so the worker resolves this provider rather than
        // the default one a bare model id would fall back to.
        let spec = format!("{id}/{model}");
        let spawner = build_real_worker_spawner(
            &spec,
            &spec,
            None,
            // The matrix measures this provider, not a learned pick.
            None,
            std::time::Duration::from_secs(cfg.pilot.task_timeout_secs),
            // First-attempt behaviour, as below: no rollback, no hygiene gate.
            0,
            false,
            cfg.pilot.sandbox.clone(),
            sandbox_avail.clone(),
            None,
        )?;
        // The id is a config key and this directory is deleted: keep it one
        // plain path component.
        let safe_id: String = id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let scratch =
            std::env::temp_dir().join(format!("wingman-validate-{}-{safe_id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let mut row = provider_matrix::run_canned_plan(&scratch, &id, &model, |base| {
            wingman_autonomous::pipeline::PipelineInputs {
                provider,
                manager_model: model.clone(),
                worker_spawner: spawner,
                base_branch: cfg.pilot.pr.base_branch.clone(),
                project_root: scratch.clone(),
                command_runner: Box::new(wingman_autonomous::pr::SystemCommandRunner),
                no_pr: true,
                orchestrator_cfg: wingman_autonomous::orchestrator::OrchestratorConfig {
                    max_concurrent_agents: 1,
                    task_timeout: std::time::Duration::from_secs(cfg.pilot.task_timeout_secs),
                    project_root: scratch.clone(),
                    run_id: provider_matrix::RUN_ID.into(),
                    base_commit: base.into(),
                    use_real_worktrees: true,
                    max_usd,
                    max_total_tokens: max_tokens,
                    // First-attempt behaviour is what the matrix reports, and
                    // retries would multiply the spend the cap is meant to bound.
                    max_retries_per_task: 0,
                    desktop_inbox: None,
                    sample_host_load: false,
                    speculative_prespawn: false,
                    warm_cmd: String::new(),
                },
                max_ticks: cfg.pilot.max_manager_ticks,
                tier: wingman_config::PilotTier::Copilot,
                worker_model: spec.clone(),
                // Validation runs stay out of the adaptive-routing history.
                stats_path: None,
                auto_approved: false,
                pr_config: cfg.pilot.pr.clone(),
                security_config: cfg.pilot.security.clone(),
                disabled_tools: cfg.tools.disabled_tools.clone(),
                run_reviewer: false,
                critic: None,
                merge_fixer: false,
                knowledge_keeper: None,
                reviewer_model: model.clone(),
                sandbox_default_tier: cfg.pilot.sandbox.default_tier.clone(),
                sandbox_availability: sandbox_avail.clone(),
                dangerous_paths: Vec::new(),
            }
        })
        .await;

        // Real spend, so `wingman cost` should see it like any pilot run.
        if let Ok(store) = RunStore::load(&run_dir(&scratch, provider_matrix::RUN_ID)).await {
            if let Ok(events) = store.read_events().await {
                let by_model = wingman_autonomous::reporting::tokens_by_model(&events);
                if !by_model.is_empty() {
                    wingman_tui::usage_store::LifetimeUsage::load().save_merged(&by_model);
                }
            }
        }
        if row.verdict == Verdict::Pass {
            let _ = std::fs::remove_dir_all(&scratch);
        } else {
            row.detail = format!("{} (scratch repo kept: {})", row.detail, scratch.display());
        }
        eprintln!(
            "[pilot] validate: {id}/{model}: {:?} (${:.4}, {} tokens) {}",
            row.verdict, row.usd, row.tokens, row.detail
        );
        rows.push(row);
    }

    let report = MatrixReport {
        generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        goal: provider_matrix::GOAL.into(),
        max_usd,
        max_total_tokens: max_tokens,
        rows,
    };
    let markdown = report.render_markdown();
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    std::fs::write(out_dir.join("matrix.md"), &markdown)?;
    std::fs::write(
        out_dir.join("matrix.json"),
        serde_json::to_string_pretty(&report)?,
    )?;
    print!("{markdown}");
    eprintln!(
        "[pilot] validate: wrote {}",
        out_dir.join("matrix.md").display()
    );
    if report.any_failed() {
        Ok(ExitCode::from(1))
    } else if !report.any_ran() {
        eprintln!("[pilot] validate: no provider could run; see the skipped reasons above.");
        Ok(ExitCode::from(2))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

/// The model to validate `id` with, or why it is skipped (with the model, when
/// one is configured).
fn validation_target(
    cfg: &Config,
    id: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<String, (Option<String>, String)> {
    let Some(pc) = cfg.providers.get(id) else {
        return Err((None, format!("no [providers.{id}] section")));
    };
    let Some(model) = pc.model.clone().filter(|m| !m.trim().is_empty()) else {
        return Err((None, format!("no model: set [providers.{id}].model")));
    };
    if let Err(why) = wingman_autonomous::provider_support::gate_run(id) {
        return Err((Some(model), why));
    }
    match runtime::missing_credential(cfg, id, env) {
        Some(why) => Err((Some(model), why)),
        None => Ok(model),
    }
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
    let line = serde_json::json!({
        "source": cand.source,
        "title": cand.title,
        "score": cand.score(),
        "action": format!("{action:?}"),
    });
    wingman_config::append_line(path, &line.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wingman_autonomous::control::{append, ControlCommand};

    /// Rollback discards a worker's edits, so it is autopilot's by default and
    /// an explicit capability wins either way; the threshold only travels to
    /// the worker while it is on.
    #[test]
    fn turn_rollback_follows_the_tier_unless_overridden() {
        let mut pilot = wingman_config::PilotConfig {
            turn_rollback_after: 3,
            ..Default::default()
        };
        assert_eq!(turn_rollback_after(&pilot), 0);
        assert!(!capability_on(&pilot, "checkpoint_hygiene"));
        assert!(capability_on(&pilot, "speculative_prespawn"));
        assert!(capability_on(&pilot, "adaptive_concurrency"));

        pilot.tier = wingman_config::PilotTier::Autopilot;
        assert_eq!(turn_rollback_after(&pilot), 3);
        assert!(capability_on(&pilot, "checkpoint_hygiene"));

        pilot.capabilities.insert("turn_rollback".into(), false);
        assert_eq!(turn_rollback_after(&pilot), 0);

        pilot.tier = wingman_config::PilotTier::Assist;
        assert!(!capability_on(&pilot, "speculative_prespawn"));
        pilot.capabilities.insert("turn_rollback".into(), true);
        assert_eq!(turn_rollback_after(&pilot), 3);
    }

    /// E4's merge-fixer follows write-set scheduling (copilot and autopilot);
    /// J8's knowledge-keeper is autopilot's, and runs on the `summarize` class
    /// when that is routed.
    #[test]
    fn merge_fixer_and_knowledge_keeper_follow_the_tier() {
        let cfg: Config = toml::from_str(
            r#"
            default_provider = "ollama"
            [providers.ollama]
            base_url = "http://localhost:11434/v1"
            [router]
            fast_model = "ollama/llama3.2"
            "#,
        )
        .unwrap();
        let manager = || wingman_autonomous::pipeline::AuxAgent {
            provider: runtime::build_provider(&cfg, "ollama").unwrap(),
            model: "manager".into(),
        };
        let mut pilot = wingman_config::PilotConfig::default();
        assert!(capability_on(&pilot, "merge_fixer"));
        assert!(knowledge_keeper(&cfg, &pilot, manager()).is_none());

        pilot.tier = wingman_config::PilotTier::Assist;
        assert!(!capability_on(&pilot, "merge_fixer"));

        pilot.tier = wingman_config::PilotTier::Autopilot;
        let keeper = knowledge_keeper(&cfg, &pilot, manager()).unwrap();
        assert_eq!(keeper.model, "manager", "summarize is unrouted");

        let mut routed = cfg.clone();
        routed
            .router
            .classes
            .insert("summarize".into(), "fast".into());
        let keeper = knowledge_keeper(&routed, &pilot, manager()).unwrap();
        assert_eq!(keeper.model, "llama3.2");

        pilot.capabilities.insert("knowledge_keeper".into(), false);
        assert!(knowledge_keeper(&routed, &pilot, manager()).is_none());
    }

    /// J10: the critic is autopilot's; `critic_model` wins over the reviewer
    /// and manager models; `critic_other_family` refuses a critic from the
    /// workers' family and one whose family the name does not tell.
    #[test]
    fn critic_follows_the_tier_and_can_require_another_family() {
        let cfg: Config = toml::from_str(
            r#"
            default_provider = "ollama"
            [providers.ollama]
            base_url = "http://localhost:11434/v1"
            "#,
        )
        .unwrap();
        let manager = || wingman_autonomous::pipeline::AuxAgent {
            provider: runtime::build_provider(&cfg, "ollama").unwrap(),
            model: "manager".into(),
        };
        let worker = "ollama/llama3.2";
        let mut pilot = wingman_config::PilotConfig::default();
        assert!(critic(&cfg, &pilot, worker, manager()).unwrap().is_none());

        pilot.tier = wingman_config::PilotTier::Autopilot;
        assert_eq!(
            critic(&cfg, &pilot, worker, manager())
                .unwrap()
                .unwrap()
                .model,
            "manager"
        );
        pilot.reviewer_model = Some("ollama/llama3.1:70b".into());
        pilot.critic_model = Some("ollama/qwen2.5-coder".into());
        assert_eq!(
            critic(&cfg, &pilot, worker, manager())
                .unwrap()
                .unwrap()
                .model,
            "qwen2.5-coder"
        );

        pilot.critic_other_family = true;
        assert!(critic(&cfg, &pilot, worker, manager()).is_ok());
        pilot.critic_model = None;
        let err = critic(&cfg, &pilot, worker, manager()).err().unwrap();
        assert!(err.to_string().contains("(meta)"), "{err}");
        pilot.critic_model = Some("ollama/house-model".into());
        let err = critic(&cfg, &pilot, worker, manager()).err().unwrap();
        assert!(err.to_string().contains("unknown family"), "{err}");

        pilot.capabilities.insert("critic".into(), false);
        assert!(critic(&cfg, &pilot, worker, manager()).unwrap().is_none());
    }

    fn migration_task() -> wingman_autonomous::Task {
        let mut t = wingman_autonomous::Task::new(
            "t-mig",
            wingman_autonomous::model::Role::Developer,
            "migrate",
        );
        t.writes = vec!["db/migrations/001.sql".into()];
        t
    }

    fn avail(docker: bool, vm: bool) -> wingman_autonomous::sandbox::TierAvailability {
        wingman_autonomous::sandbox::TierAvailability {
            docker,
            vm: if vm { Ok(()) } else { Err("no kvm".into()) },
        }
    }

    #[test]
    fn vm_tasks_are_refused_unless_isolated_or_opted_out() {
        let mut cfg = wingman_config::PilotSandboxConfig::default();
        let plain = wingman_autonomous::Task::new(
            "t-edit",
            wingman_autonomous::model::Role::Developer,
            "edit",
        );
        // Docker alone is not a vm: still refused.
        assert!(refuse_unisolated_vm_tasks(
            &cfg,
            &avail(true, false),
            &[migration_task()]
        ));
        assert!(!refuse_unisolated_vm_tasks(
            &cfg,
            &avail(false, true),
            &[migration_task()]
        ));
        assert!(!refuse_unisolated_vm_tasks(
            &cfg,
            &avail(false, false),
            &[plain]
        ));
        cfg.allow_unsandboxed_vm_tasks = true;
        assert!(!refuse_unisolated_vm_tasks(
            &cfg,
            &avail(false, false),
            &[migration_task()]
        ));
    }

    #[test]
    fn tool_synthesis_is_off_below_autopilot_and_gated_without_trust() {
        use wingman_autonomous::approval::ApprovalTier;
        let untrusted = std::path::Path::new("no-such-project/.wingman/config.toml");
        let mut pilot = wingman_config::PilotConfig::default();
        assert_eq!(tool_synthesis_for(&pilot, untrusted), None);
        pilot.tier = wingman_config::PilotTier::Autopilot;
        assert_eq!(
            tool_synthesis_for(&pilot, untrusted),
            Some(ApprovalTier::Hard)
        );
        pilot.tier = wingman_config::PilotTier::Copilot;
        pilot.capabilities.insert("tool_synthesis".into(), true);
        assert_eq!(
            tool_synthesis_for(&pilot, untrusted),
            Some(ApprovalTier::Hard)
        );
    }

    #[test]
    fn each_worker_gets_the_sandbox_its_tier_resolves_to() {
        use wingman_autonomous::sandbox::IsolationTier;
        let cfg = wingman_config::PilotSandboxConfig::default();
        let plain = wingman_autonomous::Task::new(
            "t-edit",
            wingman_autonomous::model::Role::Developer,
            "edit",
        );
        assert!(worker_sandbox_for(&plain, &cfg, &avail(true, true))
            .unwrap()
            .is_none());
        let tier = |cfg: &wingman_config::PilotSandboxConfig, t: &wingman_autonomous::Task, a| {
            worker_sandbox_for(t, cfg, &a).map(|s| s.map(|s| s.tier))
        };
        assert_eq!(
            tier(&cfg, &migration_task(), avail(true, true)),
            Ok(Some(IsolationTier::Vm))
        );
        // A vm task that reaches a worker without a vm backend (added
        // mid-run, past the start gate) is refused, not degraded...
        let refused = tier(&cfg, &migration_task(), avail(true, false)).unwrap_err();
        assert!(refused.contains("t-mig"), "{refused}");
        // ...unless the operator opted in.
        let mut opted = cfg.clone();
        opted.allow_unsandboxed_vm_tasks = true;
        assert_eq!(
            tier(&opted, &migration_task(), avail(true, false)),
            Ok(Some(IsolationTier::Container))
        );
        assert_eq!(
            tier(&opted, &migration_task(), avail(false, false)),
            Ok(None)
        );

        let mut floor = cfg.clone();
        floor.default_tier = "container".into();
        let sb = worker_sandbox_for(&plain, &floor, &avail(true, false))
            .unwrap()
            .unwrap();
        assert_eq!(sb.tier, IsolationTier::Container);
        assert_eq!(sb.config.container_image, floor.container_image);
    }

    /// A worker transcript on disk becomes a row, its files land in the
    /// union table ahead of the footer, and a key in the goal is redacted.
    #[test]
    fn a_run_exports_as_a_pr_description_with_its_workers() {
        use wingman_autonomous::model::{Agent, AgentStatus, Role, Task};

        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("pilot-r1-a1.jsonl"),
            [
                r#"{"kind":"user","ts":"t","text":"do t1"}"#,
                r#"{"kind":"assistant","ts":"t","blocks":[{"type":"tool_use","id":"x","name":"write_file","input":{"path":"src/a.rs","content":"one\ntwo"}}]}"#,
                r#"{"kind":"tool_result","ts":"t","id":"x","output":"wrote","is_error":false}"#,
                r#"{"kind":"usage_delta","ts":"t","usage":{"input_tokens":10,"output_tokens":5}}"#,
                r#"{"kind":"stop","ts":"t","reason":"end_turn","verified":true}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        let mut state = wingman_autonomous::RunState::new(
            "r1",
            "ship it with sk-abcdefghij0123456789ABCDEF",
            "0123456789abcdef",
            "wingman/r1",
        );
        let mut task = Task::new("t1", Role::Developer, "write a");
        task.agent = Some("a1".into());
        state.tasks.push(task);
        for (id, session) in [
            ("a1", Some("pilot-r1-a1")),
            ("a2", Some("gone")),
            ("a3", None),
        ] {
            state.agents.push(Agent {
                id: id.into(),
                name: if id == "a1" {
                    "brave_otter".into()
                } else {
                    String::new()
                },
                role: Role::Developer,
                current_task: None,
                pid: None,
                status: AgentStatus::Done,
                session_id: session.map(str::to_string),
                spawned_at: None,
                current_tool: None,
                usd: 0.0,
                model: None,
            });
        }

        let workers = worker_exports(&state, &sessions);
        assert_eq!(workers.len(), 1, "missing transcripts are skipped");
        assert_eq!(workers[0].task.as_deref(), Some("t1"));

        let (body, redacted) = render_run_export(&state, &workers);
        assert_eq!(redacted, 1);
        assert!(!body.contains("sk-abcdefghij"), "{body}");
        assert!(
            body.contains(
                "| brave_otter | t1 | +2 −0 across 1 file | 1 of 1 green, last passed | 15 |"
            ),
            "{body}"
        );
        assert!(body.contains("| `src/a.rs` | 2 | 0 |"), "{body}");
        let files = body.find("## Files changed").unwrap();
        assert!(
            files < body.find("_Opened by wingman pilot").unwrap(),
            "{body}"
        );
    }

    #[test]
    fn r4_eval_gate_flags_regression_and_passes_on_parity() {
        use wingman_autonomous::eval::EvalResult;
        let good = |g: &str| EvalResult {
            goal: g.into(),
            success: true,
            usd: 0.10,
            wall_min: 1.0,
            quality: 1.0,
            judged: g == "a",
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
        assert!(
            report.contains("golden reference for 1/2 goal(s) (baseline: 1/2)"),
            "{report}"
        );

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

    /// R2 in the daemon: a pass records each terminal PR once, and the daemon
    /// runs one on its first cycle and then every `feedback_poll_secs`.
    #[tokio::test]
    async fn r2_daemon_polls_feedback_on_its_cadence() {
        use std::time::{Duration, Instant};
        use wingman_autonomous::model::Event;
        use wingman_autonomous::pr::{CommandOut, CommandRunner};
        use wingman_autonomous::store::RunStore;

        struct MergedGh;
        impl CommandRunner for MergedGh {
            fn run(
                &self,
                program: &str,
                args: &[&str],
                _cwd: &std::path::Path,
            ) -> std::io::Result<CommandOut> {
                assert_eq!((program, &args[..2]), ("gh", &["pr", "view"][..]));
                Ok(CommandOut {
                    status: Some(0),
                    stdout: r#"{"state":"MERGED"}"#.into(),
                    stderr: String::new(),
                })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join(".wingman").join("autonomous").join("r1");
        let mut store = RunStore::create(&run, "r1", "g", "base", "wingman/auto/r1")
            .await
            .unwrap();
        store
            .append(Event::RunPr {
                t: RunStore::now(),
                url: "https://gh/pr/1".into(),
            })
            .await
            .unwrap();
        drop(store);

        assert_eq!(poll_feedback(&MergedGh, dir.path(), 0).await, 1);
        assert_eq!(
            poll_feedback(&MergedGh, dir.path(), 1).await,
            0,
            "an outcome is recorded once"
        );

        let t0 = Instant::now();
        assert!(feedback_due(None, t0, 3600));
        assert!(!feedback_due(None, t0, 0), "0 turns it off");
        let later = t0 + Duration::from_secs(3599);
        assert!(!feedback_due(Some(t0), later, 3600));
        assert!(feedback_due(Some(t0), later + Duration::from_secs(1), 3600));
        assert_eq!(
            wingman_config::PilotDaemonConfig::default().feedback_poll_secs,
            3600
        );
    }

    /// R4: a goal's run leaves the checkout on its integration branch; the
    /// suite returns it to the branch (or detached commit) it started on.
    #[test]
    fn r4_eval_returns_the_checkout_where_it_found_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
        };
        if git(&["init", "-q", "-b", "trunk"]).is_err() {
            eprintln!("skipping: git not available");
            return;
        }
        for kv in [["user.email", "t@t.t"], ["user.name", "t"]] {
            git(&["config", kv[0], kv[1]]).unwrap();
        }
        std::fs::write(root.join("a.txt"), "a").unwrap();
        git(&["add", "-A"]).unwrap();
        git(&["commit", "-qm", "base"]).unwrap();
        let runner = wingman_autonomous::pr::SystemCommandRunner;
        let home = current_checkout(&runner, root).expect("on a branch");
        assert_eq!(home, "trunk");

        // What a run does: switch to a rebuilt integration branch.
        git(&["switch", "-q", "-c", "wingman/auto/r1"]).unwrap();
        std::fs::remove_file(root.join("a.txt")).unwrap();
        git(&["commit", "-qam", "run"]).unwrap();
        restore_checkout(&runner, root, &home).unwrap();
        assert_eq!(current_checkout(&runner, root).as_deref(), Some("trunk"));
        assert!(root.join("a.txt").exists());

        // Detached: comes back as the commit.
        git(&["checkout", "-q", "--detach", "trunk"]).unwrap();
        let detached = current_checkout(&runner, root).unwrap();
        assert_eq!(detached.len(), 40, "{detached}");
        git(&["switch", "-q", "wingman/auto/r1"]).unwrap();
        restore_checkout(&runner, root, &detached).unwrap();
        assert_eq!(current_checkout(&runner, root), Some(detached));
        assert!(restore_checkout(&runner, root, "no-such-branch").is_err());
    }

    /// R4: the judge runs on the `judge` class when routed, else on the
    /// planner model.
    #[test]
    fn r4_eval_judge_routes_through_the_judge_class() {
        let cfg: Config = toml::from_str(
            r#"
            default_provider = "ollama"
            default_model = "ollama/qwen2.5-coder"
            [providers.ollama]
            base_url = "http://localhost:11434/v1"
            [router]
            fast_model = "ollama/llama3.2"
            "#,
        )
        .unwrap();
        assert_eq!(eval_judge(&cfg).unwrap().model, "qwen2.5-coder");

        let mut routed = cfg.clone();
        routed.router.classes.insert("judge".into(), "fast".into());
        assert_eq!(eval_judge(&routed).unwrap().model, "llama3.2");
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

#[cfg(test)]
mod ask_tests {
    use super::answers;

    /// `pilot ask` finds its reply by reading the run's own event log — the
    /// same `worker_msg:` events every other worker message lands in. If the
    /// filter widened to all worker messages, an `ack` or a `question` would
    /// be printed as though it were the answer.
    #[test]
    fn only_answer_messages_are_picked_out_of_the_event_log() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path();
        let mut body = String::new();
        // Build the line with serde rather than by hand: the tool field holds
        // nested JSON, and hand-escaping it is how this test lies to itself.
        let ev = |tool: &str| {
            serde_json::json!({
                "ev": "task.tool",
                "t": "2026-01-01T00:00:00Z",
                "id": "t1",
                "agent": "a1",
                "tool": tool,
                "ok": true,
            })
            .to_string()
        };
        // An ordinary tool call, a non-answer worker message, then two answers.
        body.push_str(&ev("read_file"));
        body.push('\n');
        body.push_str(&ev(r#"worker_msg:{"msg":"ack","command":"note"}"#));
        body.push('\n');
        body.push_str(&ev(
            r#"worker_msg:{"msg":"answer","text":"because v2 changed the shape"}"#,
        ));
        body.push('\n');
        body.push_str(&ev(r#"worker_msg:{"msg":"answer","text":"second one"}"#));
        body.push('\n');
        std::fs::write(run.join("tasks.jsonl"), body).unwrap();

        assert_eq!(
            answers(run),
            vec![
                "because v2 changed the shape".to_string(),
                "second one".to_string()
            ]
        );
    }

    #[test]
    fn a_run_with_no_events_yields_no_answers() {
        let dir = tempfile::tempdir().unwrap();
        assert!(answers(dir.path()).is_empty());
    }
}

#[cfg(test)]
mod validate_tests {
    use super::*;

    #[test]
    fn validation_skips_providers_it_cannot_run_and_says_why() {
        let mut cfg = Config::default();
        let section = |model: Option<&str>, key: Option<&str>| wingman_config::ProviderConfig {
            model: model.map(Into::into),
            api_key: key.map(Into::into),
            ..Default::default()
        };
        cfg.providers
            .insert("anthropic".into(), section(Some("claude-x"), Some("sk")));
        cfg.providers
            .insert("openai".into(), section(Some("gpt-x"), None));
        cfg.providers.insert("ollama".into(), section(None, None));
        let no_env = |_: &str| None;

        assert_eq!(
            validation_target(&cfg, "anthropic", &no_env),
            Ok("claude-x".into())
        );
        let (model, why) = validation_target(&cfg, "openai", &no_env).unwrap_err();
        assert_eq!(model.as_deref(), Some("gpt-x"));
        assert!(why.contains("OPENAI_API_KEY"), "{why}");
        let (_, why) = validation_target(&cfg, "ollama", &no_env).unwrap_err();
        assert!(why.contains("[providers.ollama].model"), "{why}");
        let (_, why) = validation_target(&cfg, "groq", &no_env).unwrap_err();
        assert!(why.contains("no [providers.groq]"), "{why}");
    }

    #[tokio::test]
    async fn validation_refuses_an_uncapped_run() {
        let err = validate_providers(Config::default(), Vec::new(), 0.0, 1000, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("above 0"), "{err}");
        let err = validate_providers(Config::default(), Vec::new(), f64::NAN, 1000, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("above 0"), "{err}");
    }
}
