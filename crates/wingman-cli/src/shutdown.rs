//! Ctrl+C / SIGTERM handling for long-running foreground runs.
//!
//! `pilot run` drives the whole pipeline in-process. Workers are spawned in
//! their own process groups (`setsid`) so they survive the parent — great for
//! `pilot abort`, but it means a bare Ctrl+C kills the manager and orphans the
//! worker/tool trees (Rust destructors don't run on signal death). This installs
//! a signal task that tree-kills every live worker group before exiting.

/// Spawn a task that force-terminates all supervised worker trees on
/// Ctrl+C (and, on Unix, SIGTERM) and then exits. Hard-stop: no graceful
/// drain — run state is persisted continuously, so an interrupted run is
/// resumable via `pilot resume`.
pub fn install() {
    tokio::spawn(async {
        wait_for_signal().await;
        eprintln!("\n[pilot] interrupt received — terminating workers…");
        wingman_autonomous::child_process::kill_all_live_groups();
        // 130 = terminated by SIGINT, the conventional shell exit code.
        std::process::exit(130);
    });
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    // If a stream can't be installed there's nothing sane to fall back to;
    // a run that can't be interrupted is worse than a panic here.
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
