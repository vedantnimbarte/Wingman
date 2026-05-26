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
            let selection = crate::runtime::resolve_selection(&cfg, cli.model.as_deref())?;
            let agent = crate::runtime::build_agent(&cfg, &selection, mode)?;
            let cfg_for_builder = cfg.clone();
            let builder: arccode_tui::ProviderBuilder = std::sync::Arc::new(move |id: &str| {
                crate::runtime::build_provider(&cfg_for_builder, id).map_err(|e| e.to_string())
            });
            let ctx = arccode_tui::AppCtx {
                provider_id: selection.provider_id,
                model: selection.model,
                mode: mode.to_string(),
                builder,
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
