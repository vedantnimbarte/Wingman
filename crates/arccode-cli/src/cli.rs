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
        Some(Command::Review { pr, local, template }) => {
            commands::review::run(pr, local, template).await
        }
        Some(Command::Discover) => commands::discover::run().await,
        Some(Command::Schedule { all }) => commands::schedule::run(all).await,
        Some(Command::Skill { action }) => match action {
            SkillAction::Extract { min, force } => commands::skill::extract(min, force).await,
        },
        Some(Command::ReviewMulti { pr, local, models }) => {
            commands::review_multi::run(pr, local, models).await
        }
        Some(Command::Diff { file, patch }) => commands::diff::run(file, patch).await,
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
                    Ok(sel) => match crate::runtime::build_agent_and_registry(&cfg, &sel, mode)
                        .await
                    {
                        Ok((a, registry)) => {
                            let mcp = std::sync::Arc::new(crate::mcp_registry::McpRegistry::new(
                                registry,
                            ));
                            mcp.seed(&cfg.mcp).await;
                            *mcp_handle.lock().expect("mcp_handle poisoned") = Some(mcp);
                            (Some(sel), Some(a))
                        }
                        Err(e) => {
                            tracing::warn!("failed to build agent at startup: {e}");
                            (Some(sel), None)
                        }
                    },
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
                        let mcp = std::sync::Arc::new(crate::mcp_registry::McpRegistry::new(
                            registry,
                        ));
                        mcp.seed(&cfg.mcp).await;
                        *mcp_slot.lock().expect("mcp_handle poisoned") = Some(mcp);
                        Ok(agent)
                    })
                });

            // Login-task runner: probes a freshly-entered key, persists
            // credentials, or runs the ChatGPT OAuth browser flow.
            let login_runner: arccode_tui::LoginRunner = std::sync::Arc::new(
                move |task: arccode_tui::modal::LoginTask| {
                    Box::pin(async move {
                        // OAuthLogin is handled here rather than in login.rs
                        // because it needs async I/O (network + local server).
                        if let arccode_tui::modal::LoginTask::OAuthLogin { ref provider_id } =
                            task
                        {
                            if provider_id == "chatgpt" {
                                return run_chatgpt_oauth().await;
                            }
                        }
                        crate::login::run_login_task(task).await
                    })
                },
            );

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
