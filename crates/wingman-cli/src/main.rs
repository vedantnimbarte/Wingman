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

/// Generous stack for the thread that parses args and drives the agent loop.
/// The default main-thread stack is only 1 MiB on Windows, which a debug build
/// can exhaust while clap constructs its (large) derived `Command` tree or while
/// deep async state machines unwind — manifesting as a bare "thread 'main' has
/// overflowed its stack" before any output. 64 MiB is free until touched.
const MAIN_STACK_SIZE: usize = 64 * 1024 * 1024;

fn main() -> ExitCode {
    logging::install_panic_handler();

    // Run everything on an explicitly large-stack thread so startup never
    // depends on the platform's default main-thread stack size.
    let spawned = std::thread::Builder::new()
        .name("wingman-main".into())
        .stack_size(MAIN_STACK_SIZE)
        .spawn(run_on_main_thread);

    let handle = match spawned {
        Ok(h) => h,
        Err(e) => {
            eprintln!("wingman: failed to spawn main worker thread: {e}");
            return ExitCode::from(2);
        }
    };

    handle.join().unwrap_or_else(|_| {
        eprintln!("wingman: main worker thread panicked");
        ExitCode::from(2)
    })
}

fn run_on_main_thread() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // Match the roomy main stack for worker threads that run the agent loop.
        .thread_stack_size(16 * 1024 * 1024)
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
