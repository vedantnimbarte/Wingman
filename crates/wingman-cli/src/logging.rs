use std::sync::OnceLock;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

static INSTALLED: OnceLock<()> = OnceLock::new();

/// Install a `tracing_subscriber` formatter writing to stderr.
///
/// Filter resolution: `WINGMAN_LOG` > flag-derived level > "info,wingman=info".
pub fn install_tracing(verbose: u8, quiet: bool) {
    if INSTALLED.set(()).is_err() {
        return;
    }

    let env_filter = std::env::var("WINGMAN_LOG")
        .ok()
        .and_then(|s| EnvFilter::try_new(s).ok())
        .unwrap_or_else(|| {
            let level = if quiet {
                "error"
            } else {
                match verbose {
                    0 => "info,wingman=info",
                    1 => "info,wingman=debug",
                    _ => "debug,wingman=trace",
                }
            };
            EnvFilter::new(level)
        });

    let fmt_layer = fmt::layer().with_target(false).with_writer(std::io::stderr);

    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .try_init();
}

/// Replace the default panic hook with one that logs via `tracing` so panics
/// land in the same sink as everything else and include a backtrace.
pub fn install_panic_handler() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(target: "wingman::panic", "{info}");
        default(info);
    }));
}
