use crate::{commands, logging};
use anyhow::Result;
use arccode_config::{global_config_path, Config, PermissionMode, ProjectPaths};
use clap::{Parser, Subcommand};
use std::process::ExitCode;

/// arccode — multi-provider terminal coding agent.
#[derive(Parser, Debug)]
#[command(
    name = "arccode",
    version,
    about = "Multi-provider terminal coding agent",
    long_about = None,
)]
pub struct Cli {
    /// Permission mode for this session: read-only | auto-edit | yolo.
    #[arg(long, value_name = "MODE", global = true)]
    pub mode: Option<String>,

    /// Model id, optionally prefixed with provider (e.g. `anthropic/claude-opus-4-7`).
    #[arg(long, value_name = "MODEL", global = true, env = "ARCCODE_MODEL")]
    pub model: Option<String>,

    /// Print a single response and exit (non-interactive).
    #[arg(long, value_name = "PROMPT")]
    pub print: Option<String>,

    /// Run prompts from a JSONL file non-interactively.
    #[arg(long, value_name = "FILE")]
    pub batch: Option<String>,

    /// Emit newline-delimited JSON events instead of text. Use with `--print`.
    #[arg(long)]
    pub json: bool,

    /// Run as a pilot-mode worker subprocess: load the role's system prompt,
    /// read the task spec from `--task-file`, run the agent loop with the
    /// task as the user prompt, emit a final `task_complete` event. Hidden
    /// from `--help`; only the orchestrator spawns workers this way.
    #[arg(long, hide = true)]
    pub worker_mode: bool,

    /// Path to a JSON file containing the [`arccode_autonomous::Task`] this
    /// worker should execute. Required with `--worker-mode`.
    #[arg(long, hide = true, value_name = "PATH")]
    pub task_file: Option<String>,

    /// Role name (e.g. `developer`, `designer`). Used to look up the role
    /// system prompt under `~/.arccode/agents/<role>.md`.
    #[arg(long, hide = true, value_name = "ROLE")]
    pub role: Option<String>,

    /// Session id under `<project>/.arccode/sessions/<id>.jsonl` for the
    /// worker's own transcript. The orchestrator records this so
    /// `arccode session fork` can target the worker's turns later.
    #[arg(long, hide = true, value_name = "ID", env = "ARCCODE_SESSION_ID")]
    pub session_id: Option<String>,

    /// Worktree path. The worker `cd`s here before running so all relative
    /// edits land inside the per-task worktree.
    #[arg(long, hide = true, value_name = "PATH")]
    pub worktree: Option<String>,

    /// Increase log verbosity (-v, -vv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Suppress non-error stderr output.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Inspect or scaffold configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Generate or refresh ARCCODE.md by introspecting the current project.
    Init {
        /// Overwrite an existing ARCCODE.md.
        #[arg(long)]
        force: bool,
    },
    /// Checkpoint the working tree into a named git stash for /undo recovery.
    Checkpoint {
        /// Optional label.
        #[arg(long)]
        label: Option<String>,
    },
    /// Restore the most recent arccode checkpoint via `git stash pop`.
    Undo,
    /// Show estimated token spend by model from ~/.arccode/usage.json.
    Cost {
        /// Output as JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Session utilities.
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// `git worktree` helper — isolate an experiment under .arccode/worktrees.
    Worktree {
        #[command(subcommand)]
        action: WorktreeAction,
    },
    /// Memory pack utilities.
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// One-shot code review of a PR or local diff.
    Review {
        /// PR number to review (uses `gh pr diff`).
        pr: Option<String>,
        /// Review local commits against this base ref instead.
        #[arg(long, value_name = "BASE")]
        local: Option<String>,
        /// Path to a custom review prompt template.
        #[arg(long, value_name = "FILE")]
        template: Option<String>,
    },
    /// Probe localhost for running Ollama / LM Studio / vLLM and print
    /// discovered models.
    Discover,
    /// Show what Arc-Code knows about this project: memories, skills,
    /// model routing, the verification gate, and index freshness.
    Knows,
    /// Run any [[schedule]] entries whose cadence is due.
    Schedule {
        /// Force-run all configured schedule entries regardless of cadence.
        #[arg(long)]
        all: bool,
    },
    /// Skill utilities.
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
    /// Multi-model code review: run review against several models in
    /// parallel and merge findings.
    ReviewMulti {
        /// PR number to review (uses `gh pr diff`).
        pr: Option<String>,
        /// Review local commits against this base ref instead.
        #[arg(long, value_name = "BASE")]
        local: Option<String>,
        /// Comma-separated list of provider/model pairs to consult.
        #[arg(long, value_name = "LIST")]
        models: String,
    },
    /// Interactive diff viewer: walk a unified diff hunk by hunk and
    /// accept or reject each one before writing the result.
    Diff {
        /// File to view the working-tree diff for (calls `git diff -- <file>`).
        file: Option<String>,
        /// Or supply a path to a unified-diff file directly.
        #[arg(long, value_name = "FILE")]
        patch: Option<String>,
    },
    /// Authenticate a provider non-interactively: probe the key and store it
    /// in the OS keyring + config. The TUI `/login` wizard equivalent.
    Login {
        /// Provider id (e.g. anthropic, openai, gemini). Omit with --list.
        provider: Option<String>,
        /// API key. If omitted, read from the provider's env var, else prompt.
        #[arg(long, value_name = "KEY")]
        api_key: Option<String>,
        /// Model id to record as this provider's default.
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
        /// Base URL override (for local servers / proxies).
        #[arg(long, value_name = "URL")]
        base_url: Option<String>,
        /// Force the browser OAuth flow (chatgpt).
        #[arg(long)]
        oauth: bool,
        /// Skip the live connectivity test before saving.
        #[arg(long)]
        no_probe: bool,
        /// Register the provider without making it the default selection.
        #[arg(long)]
        no_default: bool,
        /// List the known provider ids and exit.
        #[arg(long)]
        list: bool,
    },
    /// Remove a provider's stored credential from the OS keyring.
    Logout {
        /// Provider id whose keyring entry to delete.
        provider: String,
    },
    /// Pilot mode: plan a multi-task goal, delegate to worker agents in
    /// isolated worktrees, converge into a PR.
    Pilot {
        #[command(subcommand)]
        action: PilotAction,
    },
    /// Deprecated alias for `arccode pilot` — kept through M3, removed at M4.
    #[command(hide = true)]
    Autonomous {
        goal: String,
        #[arg(long)]
        plan_only: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        review: bool,
        #[arg(long)]
        no_pr: bool,
        #[arg(long, value_name = "REV")]
        base: Option<String>,
        #[arg(long, value_name = "N")]
        max_agents: Option<u32>,
        #[arg(long, value_name = "FLOAT")]
        max_usd: Option<f64>,
    },
}

#[derive(Subcommand, Debug)]
pub enum PilotAction {
    /// Plan a goal, spawn workers, open a PR.
    Run {
        /// High-level objective in natural language.
        goal: String,
        /// Capability tier override.
        #[arg(long, value_name = "TIER")]
        tier: Option<String>,
        /// Plan and write tasks.jsonl, do not spawn workers.
        #[arg(long)]
        plan_only: bool,
        /// Auto-approve the plan (no interactive gate).
        #[arg(long)]
        yes: bool,
        /// Force hard plan-approval gate regardless of tier.
        #[arg(long)]
        review: bool,
        /// Tail the run in this terminal.
        #[arg(long)]
        watch: bool,
        /// Skip `gh pr create` (just push the branch).
        #[arg(long)]
        no_pr: bool,
        /// Branch from <REV> instead of HEAD.
        #[arg(long, value_name = "REV")]
        base: Option<String>,
        /// Override pilot.max_concurrent_agents.
        #[arg(long, value_name = "N")]
        max_agents: Option<u32>,
        /// Override pilot.max_usd cost cap.
        #[arg(long, value_name = "FLOAT")]
        max_usd: Option<f64>,
        /// Override sandbox tier per run (host | container | vm).
        #[arg(long, value_name = "TIER")]
        sandbox: Option<String>,
        /// Notification channel for plan/completion notices.
        #[arg(long, value_name = "CHANNEL")]
        channel: Option<String>,
        /// For a headless hard-gate run, wait for `pilot approve` / `pilot
        /// veto` (or the watch UI) via the control channel instead of
        /// refusing outright. Times out to a rejection.
        #[arg(long)]
        await_approval: bool,
        /// Seconds to wait for `--await-approval` before rejecting the plan.
        #[arg(long, value_name = "SECS", default_value_t = 600)]
        approval_timeout: u64,
    },
    /// Print the latest run's status as ASCII; exits immediately.
    Status {
        /// Specific run id; defaults to the most recently updated.
        run_id: Option<String>,
    },
    /// Live-watch a run: redraw whenever its state.json changes.
    Watch {
        /// Specific run id; defaults to the most recently updated.
        run_id: Option<String>,
        /// Poll interval in milliseconds.
        #[arg(long, default_value_t = 250)]
        interval_ms: u64,
        /// Force plain-ASCII glyphs (for terminals that can't render the
        /// unicode status/spinner glyphs). Auto-detected when omitted.
        #[arg(long)]
        ascii: bool,
    },
    /// Resume an interrupted run.
    ///
    /// Loads the run's existing tasks.jsonl + state.json, marks any tasks
    /// stuck in InProgress (whose worker is gone) as Failed so the retry
    /// watchdog picks them up, and resumes the manager loop from there.
    Resume {
        /// Run id under `<project>/.arccode/autonomous/`.
        run_id: String,
        /// Skip `gh pr create` (just push the branch on completion).
        #[arg(long)]
        no_pr: bool,
    },
    /// Always-on discovery daemon (J2): poll configured sources, score
    /// candidates, and surface/queue work. Requires `[pilot.daemon].enabled`.
    Daemon {
        /// Run this many discovery cycles then exit; 0 = run forever.
        #[arg(long, default_value_t = 0)]
        cycles: usize,
    },
    /// Abort a live run (or one of its tasks) via the control channel.
    Abort {
        /// Run id; defaults to the most recently updated.
        run_id: Option<String>,
        /// Abort just this task instead of the whole run.
        #[arg(long, value_name = "TASK")]
        task: Option<String>,
    },
    /// Retry a failed/blocked task in a live run.
    Retry {
        /// Task id to retry.
        task: String,
        /// Run id; defaults to the most recently updated.
        run_id: Option<String>,
    },
    /// Approve a run waiting at the plan-approval gate.
    Approve {
        /// Run id; defaults to the most recently updated.
        run_id: Option<String>,
    },
    /// Reject a run waiting at the plan-approval gate.
    Veto {
        /// Run id; defaults to the most recently updated.
        run_id: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum SkillAction {
    /// Scan recent sessions and write proposed skill drafts under
    /// ~/.arccode/skills/proposed/.
    Extract {
        /// Minimum number of distinct sessions a pattern must appear in.
        #[arg(long, default_value_t = 2)]
        min: usize,
        /// Overwrite existing draft files.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum WorktreeAction {
    /// Create a worktree under <project>/.arccode/worktrees/<branch>.
    Create { branch: String },
    /// List git worktrees (calls `git worktree list`).
    List,
    /// Remove a worktree by path.
    Remove { path: String },
}

#[derive(Subcommand, Debug)]
pub enum MemoryAction {
    /// Export the global memory directory to a target directory or single JSON pack file.
    Export { out: String },
    /// Import a memory pack (directory or JSON pack file) into the global memory directory.
    Import {
        path: String,
        /// Overwrite existing entries with the same name.
        #[arg(long)]
        force: bool,
    },
    /// Show a unified diff of two memory packs (or the live dir vs. a pack).
    Diff { a: String, b: String },
}

#[derive(Subcommand, Debug)]
pub enum SessionAction {
    /// List recent sessions for this project.
    List {
        /// Maximum number of entries.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Fork an existing session JSONL into a new file (optionally truncated).
    Fork {
        /// Path to the session JSONL to fork.
        src: String,
        /// Truncate the new file to the first N records.
        #[arg(long)]
        at: Option<usize>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Write a starter `~/.arccode/config.toml`.
    Init {
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Print the merged effective configuration.
    Show {
        /// Output as JSON instead of TOML.
        #[arg(long)]
        json: bool,
    },
    /// Print the resolved config file paths.
    Paths,
}

pub async fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    // Suppress INFO logs during TUI mode (no verbose flag) so stderr output
    // doesn't bleed into the alternate-screen buffer and corrupt the display.
    let is_tui = cli.command.is_none() && cli.print.is_none() && cli.batch.is_none();
    let quiet_for_logging = cli.quiet || (is_tui && cli.verbose == 0);
    logging::install_tracing(cli.verbose, quiet_for_logging);

    if cli.worker_mode {
        let cfg = load_config()?;
        let opts = commands::worker::WorkerOptions {
            task_file: cli
                .task_file
                .ok_or_else(|| anyhow::anyhow!("--worker-mode requires --task-file"))?,
            role: cli
                .role
                .ok_or_else(|| anyhow::anyhow!("--worker-mode requires --role"))?,
            session_id: cli.session_id,
            worktree: cli.worktree,
            model_override: cli.model,
        };
        return commands::worker::run(cfg, opts).await;
    }

    if let Some(file) = cli.batch {
        let cfg = load_config()?;
        let mode_override = parse_mode(cli.mode.as_deref())?;
        let opts = commands::batch::BatchOptions {
            file,
            json: cli.json,
            mode_override,
            model_override: cli.model,
        };
        return commands::batch::run(cfg, opts).await;
    }

    if let Some(prompt) = cli.print {
        let cfg = load_config()?;
        let mode_override = parse_mode(cli.mode.as_deref())?;
        let opts = commands::headless::HeadlessOptions {
            prompt,
            json: cli.json,
            mode_override,
            model_override: cli.model,
        };
        return commands::headless::run(cfg, opts).await;
    }

    match cli.command {
        Some(Command::Config { action }) => commands::config::run(action).await,
        Some(Command::Init { force }) => commands::init::run(force).await,
        Some(Command::Checkpoint { label }) => commands::checkpoint::create(label).await,
        Some(Command::Undo) => commands::checkpoint::undo().await,
        Some(Command::Cost { json }) => commands::cost::run(json).await,
        Some(Command::Session { action }) => commands::session::run(action).await,
        Some(Command::Worktree { action }) => match action {
            WorktreeAction::Create { branch } => commands::worktree::create(branch).await,
            WorktreeAction::List => commands::worktree::list().await,
            WorktreeAction::Remove { path } => commands::worktree::remove(path).await,
        },
        Some(Command::Memory { action }) => match action {
            MemoryAction::Export { out } => commands::memory::export(out).await,
            MemoryAction::Import { path, force } => commands::memory::import(path, force).await,
            MemoryAction::Diff { a, b } => commands::memory::diff(a, b).await,
        },
        Some(Command::Review {
            pr,
            local,
            template,
        }) => commands::review::run(pr, local, template).await,
        Some(Command::Login {
            provider,
            api_key,
            model,
            base_url,
            oauth,
            no_probe,
            no_default,
            list,
        }) => {
            commands::login::run(commands::login::LoginOptions {
                provider,
                api_key,
                model,
                base_url,
                oauth,
                no_probe,
                no_default,
                list,
            })
            .await
        }
        Some(Command::Logout { provider }) => commands::login::logout(provider).await,
        Some(Command::Discover) => commands::discover::run().await,
        Some(Command::Knows) => commands::knows::run(load_config()?).await,
        Some(Command::Schedule { all }) => commands::schedule::run(all).await,
        Some(Command::Skill { action }) => match action {
            SkillAction::Extract { min, force } => commands::skill::extract(min, force).await,
        },
        Some(Command::ReviewMulti { pr, local, models }) => {
            commands::review_multi::run(pr, local, models).await
        }
        Some(Command::Diff { file, patch }) => commands::diff::run(file, patch).await,
        Some(Command::Pilot { action }) => match action {
            PilotAction::Run {
                goal,
                tier,
                plan_only,
                yes,
                review,
                watch,
                no_pr,
                base,
                max_agents,
                max_usd,
                sandbox,
                channel,
                await_approval,
                approval_timeout,
            } => {
                let cfg = load_config()?;
                commands::pilot::run(
                    cfg,
                    commands::pilot::PilotOptions {
                        goal,
                        tier,
                        plan_only,
                        yes,
                        review,
                        watch,
                        no_pr,
                        base,
                        max_agents,
                        max_usd,
                        sandbox,
                        channel,
                        await_approval,
                        approval_timeout_secs: approval_timeout,
                        model_override: cli.model,
                    },
                )
                .await
            }
            PilotAction::Status { run_id } => commands::pilot::status(run_id).await,
            PilotAction::Watch {
                run_id,
                interval_ms,
                ascii,
            } => {
                commands::pilot::watch(run_id, interval_ms, commands::pilot::resolve_ascii(ascii))
                    .await
            }
            PilotAction::Resume { run_id, no_pr } => {
                let cfg = load_config()?;
                commands::pilot::resume(cfg, run_id, no_pr, cli.model).await
            }
            PilotAction::Daemon { cycles } => {
                let cfg = load_config()?;
                commands::pilot::daemon(cfg, cycles).await
            }
            PilotAction::Abort { run_id, task } => {
                commands::pilot::control_abort(run_id, task).await
            }
            PilotAction::Retry { task, run_id } => {
                commands::pilot::control_retry(run_id, task).await
            }
            PilotAction::Approve { run_id } => commands::pilot::control_approve(run_id).await,
            PilotAction::Veto { run_id } => commands::pilot::control_veto(run_id).await,
        },
        Some(Command::Autonomous {
            goal,
            plan_only,
            yes,
            review,
            no_pr,
            base,
            max_agents,
            max_usd,
        }) => {
            eprintln!(
                "[warn] `arccode autonomous` is deprecated and will be removed at M4 — use `arccode pilot` instead."
            );
            let cfg = load_config()?;
            commands::pilot::run(
                cfg,
                commands::pilot::PilotOptions {
                    goal,
                    tier: None,
                    plan_only,
                    yes,
                    review,
                    watch: false,
                    no_pr,
                    base,
                    max_agents,
                    max_usd,
                    sandbox: None,
                    channel: None,
                    await_approval: false,
                    approval_timeout_secs: 600,
                    model_override: cli.model,
                },
            )
            .await
        }
        None => {
            let cfg = load_config()?;
            let mode_override = parse_mode(cli.mode.as_deref())?;
            let mode = mode_override.unwrap_or(cfg.permission_mode);

            // Shared, replaceable MCP registry handle. Filled at startup
            // (if there's an agent), or by the agent_builder after /login.
            let mcp_handle: std::sync::Arc<
                std::sync::Mutex<Option<std::sync::Arc<crate::mcp_registry::McpRegistry>>>,
            > = std::sync::Arc::new(std::sync::Mutex::new(None));

            // Try to resolve a provider/model and build the agent. If no
            // provider is configured (or the configured one fails to build),
            // we still open the TUI — the user can run /login to set one up.
            let (selection, agent) =
                match crate::runtime::resolve_selection(&cfg, cli.model.as_deref()) {
                    Ok(sel) => {
                        match crate::runtime::build_agent_and_registry(&cfg, &sel, mode).await {
                            Ok((a, registry)) => {
                                let mcp = std::sync::Arc::new(
                                    crate::mcp_registry::McpRegistry::new(registry),
                                );
                                mcp.seed(&cfg.mcp).await;
                                *mcp_handle.lock().expect("mcp_handle poisoned") = Some(mcp);
                                (Some(sel), Some(a))
                            }
                            Err(e) => {
                                tracing::warn!("failed to build agent at startup: {e}");
                                (Some(sel), None)
                            }
                        }
                    }
                    Err(e) => {
                        tracing::info!("no provider configured: {e}");
                        (None, None)
                    }
                };

            // Kick off background indexing for the project. The handle is
            // held until the TUI exits.
            let project = ProjectPaths::discover(&std::env::current_dir()?);
            let _watch_handle = match crate::runtime::build_indexer(&project)? {
                Some(indexer) => {
                    arccode_rag::spawn_background_indexer(indexer, project.root.clone())
                        .map_err(anyhow::Error::msg)
                        .ok()
                }
                None => None,
            };

            let cfg_for_builder = cfg.clone();
            let builder: arccode_tui::ProviderBuilder = std::sync::Arc::new(move |id: &str| {
                crate::runtime::build_provider(&cfg_for_builder, id).map_err(|e| e.to_string())
            });

            // Agent builder closure used by /login to construct a fresh
            // AgentLoop after the user finishes the wizard. Re-loads config
            // from disk each call so the wizard's keyring marker and
            // default-provider updates are picked up, and rebuilds the
            // MCP registry against the new ToolRegistry.
            let agent_builder_mcp = mcp_handle.clone();
            let agent_builder: arccode_tui::AgentBuilder =
                std::sync::Arc::new(move |provider_id: String, model: String| {
                    let mcp_slot = agent_builder_mcp.clone();
                    Box::pin(async move {
                        let cfg = load_config().map_err(|e| e.to_string())?;
                        let sel = crate::runtime::Selection { provider_id, model };
                        let (agent, registry) =
                            crate::runtime::build_agent_and_registry(&cfg, &sel, mode)
                                .await
                                .map_err(|e| e.to_string())?;
                        let mcp =
                            std::sync::Arc::new(crate::mcp_registry::McpRegistry::new(registry));
                        mcp.seed(&cfg.mcp).await;
                        *mcp_slot.lock().expect("mcp_handle poisoned") = Some(mcp);
                        Ok(agent)
                    })
                });

            // Login-task runner: probes a freshly-entered key, persists
            // credentials, or runs the ChatGPT OAuth browser flow.
            let login_runner: arccode_tui::LoginRunner =
                std::sync::Arc::new(move |task: arccode_tui::modal::LoginTask| {
                    Box::pin(async move {
                        // OAuthLogin is handled here rather than in login.rs
                        // because it needs async I/O (network + local server).
                        if let arccode_tui::modal::LoginTask::OAuthLogin { ref provider_id } = task
                        {
                            if provider_id == "chatgpt" {
                                return run_chatgpt_oauth().await;
                            }
                        }
                        crate::login::run_login_task(task).await
                    })
                });

            // Logout: clear a provider's keyring entry. Sync — the keyring
            // call is fast and blocking.
            let logout_runner: arccode_tui::LogoutRunner =
                std::sync::Arc::new(|provider_id: String| {
                    arccode_config::secrets::delete(&provider_id).map_err(|e| e.to_string())
                });

            // MCP task runner — dispatches the modal's `McpTask` against
            // the live registry, returning a friendly error if no registry
            // exists yet (i.e. user hasn't completed /login).
            let mcp_handle_for_run = mcp_handle.clone();
            let mcp_runner: arccode_tui::McpRunner = std::sync::Arc::new(move |task| {
                let mcp = mcp_handle_for_run
                    .lock()
                    .expect("mcp_handle poisoned")
                    .clone();
                Box::pin(async move {
                    let Some(mcp) = mcp else {
                        return Err("MCP needs an active provider — run /login first".into());
                    };
                    match task {
                        arccode_tui::modal::McpTask::Add(p) => {
                            let cfg = arccode_config::McpServerConfig {
                                transport: "stdio".into(),
                                command: Some(p.command),
                                args: p.args,
                                url: None,
                            };
                            mcp.add(p.name, cfg).await
                        }
                        arccode_tui::modal::McpTask::Remove(n) => mcp.remove(&n).await,
                        arccode_tui::modal::McpTask::Connect(n) => mcp.connect(&n).await,
                        arccode_tui::modal::McpTask::Disconnect(n) => mcp.disconnect(&n).await,
                    }
                })
            });

            // MCP list runner — returns the current snapshot.
            let mcp_handle_for_list = mcp_handle.clone();
            let mcp_list_runner: arccode_tui::McpListRunner = std::sync::Arc::new(move || {
                let mcp = mcp_handle_for_list
                    .lock()
                    .expect("mcp_handle poisoned")
                    .clone();
                Box::pin(async move {
                    let Some(mcp) = mcp else {
                        return Vec::new();
                    };
                    mcp.list()
                        .await
                        .into_iter()
                        .map(|v| arccode_tui::modal::McpServerSummary {
                            name: v.name,
                            command: format!(
                                "{} {}",
                                v.command.unwrap_or_default(),
                                v.args.join(" ")
                            )
                            .trim()
                            .to_string(),
                            connected: v.connected,
                            tool_count: v.tool_names.len(),
                        })
                        .collect()
                })
            });

            let (provider_id, model) = selection
                .as_ref()
                .map(|s| (s.provider_id.clone(), s.model.clone()))
                .unwrap_or_default();
            let ctx = arccode_tui::AppCtx {
                provider_id,
                model,
                mode: mode.to_string(),
                project_root: project.root.clone(),
                builder,
                agent_builder,
                login_runner,
                logout_runner,
                mcp_runner,
                mcp_list_runner,
            };
            arccode_tui::init_theme(&cfg.tui);
            arccode_tui::run(agent, ctx).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn load_config() -> Result<Config> {
    let global = global_config_path()?;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let project_file = if project.config_file.exists() {
        Some(project.config_file)
    } else {
        None
    };
    Ok(Config::load(Some(&global), project_file.as_deref())?)
}

/// Run the ChatGPT OAuth PKCE flow and store the resulting tokens in the
/// OS keychain.  Called by the login runner when the user picks
/// "ChatGPT (subscription)" in the `/login` wizard.
async fn run_chatgpt_oauth() -> Result<(), String> {
    let (access_token, refresh_token) = crate::oauth::chatgpt_oauth_login()
        .await
        .map_err(|e| format!("OAuth login failed: {e}"))?;

    arccode_config::secrets::store("chatgpt", &access_token)
        .map_err(|e| format!("keyring (access token): {e}"))?;
    arccode_config::secrets::store("chatgpt_refresh", &refresh_token)
        .map_err(|e| format!("keyring (refresh token): {e}"))?;

    Ok(())
}

fn parse_mode(s: Option<&str>) -> Result<Option<PermissionMode>> {
    match s {
        None => Ok(None),
        Some(s) => s
            .parse::<PermissionMode>()
            .map(Some)
            .map_err(|e| anyhow::anyhow!(e)),
    }
}
