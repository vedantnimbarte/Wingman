//! arccode — multi-provider terminal coding agent.
//!
//! M0 surface: `--version`, `config init`, `config show`. Default subcommand
//! (TUI) prints a placeholder until M1 lands.

mod cli;
mod commands;
mod login;
mod logging;
mod mcp_adapter;
mod mcp_registry;
mod runtime;

use std::process::ExitCode;

fn main() -> ExitCode {
    logging::install_panic_handler();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("arccode: failed to start tokio runtime: {e}");
            return ExitCode::from(2);
        }
    };

    match runtime.block_on(cli::run()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("arccode: {e:#}");
            ExitCode::from(1)
        }
    }
}
