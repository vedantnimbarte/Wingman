//! wingman — multi-provider terminal coding agent.
//!
//! Binary entry point: builds the tokio runtime and dispatches to
//! [`cli::run`]. The default subcommand opens the interactive TUI; see
//! `cli.rs` for the full command surface.

mod cli;
mod commands;
mod logging;
mod login;
mod mcp_adapter;
mod mcp_registry;
mod oauth;
mod runtime;
mod shutdown;

use std::process::ExitCode;

fn main() -> ExitCode {
    logging::install_panic_handler();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("wingman: failed to start tokio runtime: {e}");
            return ExitCode::from(2);
        }
    };

    match runtime.block_on(cli::run()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wingman: {e:#}");
            ExitCode::from(1)
        }
    }
}
