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
use arccode_config::{Config, PilotConfig, ProjectPaths};

use crate::runtime;

/// Options forwarded from the clap subcommand.
pub struct PilotOptions {
    pub goal: String,
    pub tier: Option<String>,
    pub plan_only: bool,
    pub yes: bool,
    pub review: bool,
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
    let plan = plan_from_goal(&llm as &dyn PlannerLlm, &opts.goal)
        .await
        .context("planner call failed")?;

    eprintln!("[pilot] proposed {} task(s) (run id: {run_id}).", plan.len());
    eprint!("\n{}", render_plan(&plan));

    // Approval flow: --yes auto-approves; --review forces hard gate; the
    // tier-aware E1 trust ladder lands in Phase 7.8 and replaces this.
    let approve = if opts.yes && !opts.review {
        true
    } else if !std::io::stdin().is_terminal() {
        // Non-interactive (CI / pipe) with no --yes: reject the plan
        // rather than block on a prompt that nobody can answer.
        eprintln!(
            "[pilot] no TTY and --yes not given — refusing to auto-approve plan."
        );
        false
    } else {
        prompt_for_approval(&plan, &opts.goal)?
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

    // Phases 3–6 land here: build the orchestrator, spawn the manager,
    // spawn workers in worktrees, merge into the integration branch, open
    // a PR. Until then, plan-only is the only complete flow.
    let _ = (opts.watch, opts.no_pr, &pilot); // hush "unused" warnings
    let _ = PilotConfig::default(); // type-import keeps the public surface honest
    eprintln!(
        "[pilot] orchestrator (Phases 3-6) not yet implemented — re-run with --plan-only \
         to skip this notice. tasks.jsonl is ready at {}.",
        store.log_path().display()
    );
    Ok(ExitCode::from(64))
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
