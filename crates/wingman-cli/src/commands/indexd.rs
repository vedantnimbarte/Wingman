//! `wingman indexd` — keep this project's semantic index warm.
//!
//! Runs an initial reindex, then watches the tree (reusing the RAG file
//! watcher) and refreshes `index.db` on change until interrupted. A pidfile at
//! `.wingman/indexd.pid` lets other processes (and `--status`) detect a live
//! daemon so a session can open with an already-warm index.
//!
//! ponytail: foreground long-running command — background it with your shell
//! (`wingman indexd &`) or a service manager. Auto-spawn from the TUI on
//! startup is the follow-up (check the pidfile, launch detached if absent).

use std::path::Path;
use std::process::ExitCode;

use anyhow::{anyhow, Result};
use wingman_config::ProjectPaths;

use crate::runtime;

pub async fn run(status: bool) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let paths = ProjectPaths::discover(&cwd);
    let pidfile = paths.dir.join("indexd.pid");

    if status {
        return report_status(&pidfile, &paths);
    }

    if let Some(pid) = live_pid(&pidfile) {
        eprintln!("wingman: indexd already running for this project (pid {pid})");
        return Ok(ExitCode::SUCCESS);
    }

    let indexer = match runtime::build_indexer(&paths)? {
        Some(i) => i,
        None => {
            eprintln!("wingman: no index available (embedder unavailable)");
            return Ok(ExitCode::FAILURE);
        }
    };

    // Write the pidfile up front so `--status` reports "running" during the
    // (possibly slow) initial index, not just after it. Cleaned up on any exit.
    write_pidfile(&pidfile)?;
    eprintln!("indexd: initial index of {} …", paths.root.display());
    // The initial index can take a while on a big repo — keep Ctrl-C
    // responsive during it so we still clean up the pidfile instead of dying
    // by default SIGINT disposition (which leaves a stale pidfile behind).
    let stats = tokio::select! {
        r = indexer.reindex_repo() => match r {
            Ok(s) => s,
            Err(e) => {
                let _ = std::fs::remove_file(&pidfile);
                return Err(anyhow!("{e}"));
            }
        },
        _ = tokio::signal::ctrl_c() => {
            let _ = std::fs::remove_file(&pidfile);
            eprintln!("\nindexd: stopped during initial index");
            return Ok(ExitCode::SUCCESS);
        }
    };
    eprintln!(
        "indexd: {} files scanned, {} indexed, {} chunks. watching for changes (Ctrl-C to stop).",
        stats.files_scanned, stats.files_indexed, stats.chunks_written
    );

    // Hold the watcher alive for the lifetime of the daemon.
    let _watch = wingman_rag::spawn_background_indexer(indexer, paths.root.clone())
        .map_err(|e| anyhow!("watcher: {e}"))?;
    // Clean up the pidfile on Ctrl-C so `--status` doesn't report a ghost.
    let _ = tokio::signal::ctrl_c().await;
    let _ = std::fs::remove_file(&pidfile);
    eprintln!("\nindexd: stopped");
    Ok(ExitCode::SUCCESS)
}

fn report_status(pidfile: &Path, paths: &ProjectPaths) -> Result<ExitCode> {
    match live_pid(pidfile) {
        Some(pid) => println!("indexd: running (pid {pid})"),
        None => println!("indexd: not running (start with `wingman indexd`)"),
    }
    if paths.index_db.exists() {
        let age = std::fs::metadata(&paths.index_db)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs());
        match age {
            Some(secs) => println!("index: {} (updated {secs}s ago)", paths.index_db.display()),
            None => println!("index: {}", paths.index_db.display()),
        }
    } else {
        println!("index: not built yet");
    }
    Ok(ExitCode::SUCCESS)
}

fn write_pidfile(pidfile: &Path) -> Result<()> {
    if let Some(parent) = pidfile.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(pidfile, std::process::id().to_string())?;
    Ok(())
}

/// Read the pidfile and return the pid if that process is actually alive.
/// Removes a stale pidfile as a side effect. Returns None if no live daemon.
fn live_pid(pidfile: &Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(pidfile).ok()?.trim().parse().ok()?;
    if process_alive(pid) {
        Some(pid)
    } else {
        let _ = std::fs::remove_file(pidfile);
        None
    }
}

#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn process_alive(_pid: u32) -> bool {
    // ponytail: no dep-free portable liveness check; treat a present pidfile as
    // live. Stale pidfiles are cleared on the next clean start.
    true
}
