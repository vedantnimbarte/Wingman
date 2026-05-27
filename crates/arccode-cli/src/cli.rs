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
    logging::install_tracing(cli.verbose, cli.quiet);

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

            // Login-task runner: probes a freshly-entered key, or persists
            // it (keyring + config). The TUI calls into this without
            // touching arccode-providers or arccode-config directly.
            let login_runner: arccode_tui::LoginRunner = std::sync::Arc::new(
                move |task: arccode_tui::modal::LoginTask| {
                    Box::pin(async move { crate::login::run_login_task(task).await })
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

fn parse_mode(s: Option<&str>) -> Result<Option<PermissionMode>> {
    match s {
        None => Ok(None),
        Some(s) => s
            .parse::<PermissionMode>()
            .map(Some)
            .map_err(|e| anyhow::anyhow!(e)),
    }
}
