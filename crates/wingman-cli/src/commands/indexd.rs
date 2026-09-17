//! `wingman indexd` — keep this project's semantic index warm.
//!
//! Runs an initial reindex, then watches the tree (reusing the RAG file
//! watcher) and refreshes `index.db` on change. `wingman indexd` runs it in
//! the foreground until interrupted; `start` re-execs that in the background
//! (log at `.wingman/indexd.log`), `stop` asks it to exit, `status` reports.
//!
//! A pidfile at `.wingman/indexd.pid` names the daemon. It is claimed with
//! `create_new`, so two starts cannot both win, and a reader checks the pid is
//! actually alive and deletes the file when it isn't — a daemon killed without
//! cleanup does not read as running forever. A session that finds a live
//! daemon uses its warm index instead of starting its own indexer.
//!
//! `stop` writes `.wingman/indexd.stop`, which the daemon polls for, rather
//! than killing the pid: there is no graceful signal for a console-less process
//! on Windows, and a pid that was recycled after a crash must never be killed.
//!
//! ponytail: liveness is "a process with that pid exists". A daemon that died
//! uncleanly and whose pid was reused reads as running until that process
//! exits; comparing the process start time to the pidfile's mtime closes it.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context, Result};
use wingman_config::ProjectPaths;

use crate::cli::IndexdAction;
use crate::runtime;

/// How long `start` waits to see the child claim the pidfile, and `stop` waits
/// for the daemon to exit, before reporting what it knows.
const START_WAIT: Duration = Duration::from_secs(5);
const STOP_WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);

pub async fn run(action: Option<IndexdAction>) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let paths = ProjectPaths::discover(&cwd);
    match action {
        None => foreground(&paths).await,
        Some(IndexdAction::Start) => start(&paths).await,
        Some(IndexdAction::Stop) => stop(&paths.dir).await,
        Some(IndexdAction::Status) => report_status(&paths),
    }
}

async fn foreground(paths: &ProjectPaths) -> Result<ExitCode> {
    let _pidfile = match claim_pidfile(&paths.dir)? {
        Ok(guard) => guard,
        Err(pid) => {
            eprintln!("wingman: indexd already running for this project (pid {pid})");
            return Ok(ExitCode::SUCCESS);
        }
    };
    // A stop request left over from a daemon that exited before reading it
    // must not stop this one.
    let stopfile = paths.dir.join("indexd.stop");
    let _ = std::fs::remove_file(&stopfile);

    let indexer = match runtime::build_indexer(paths)? {
        Some(i) => i,
        None => {
            eprintln!("wingman: no index available (embedder unavailable)");
            return Ok(ExitCode::FAILURE);
        }
    };

    eprintln!("indexd: initial index of {} …", paths.root.display());
    // The initial index can take a while on a big repo; stay stoppable during
    // it. The pidfile guard cleans up on every return path.
    let stats = tokio::select! {
        r = indexer.reindex_repo() => r.map_err(|e| anyhow!("{e}"))?,
        _ = stop_requested(&stopfile) => {
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
    stop_requested(&stopfile).await;
    eprintln!("\nindexd: stopped");
    Ok(ExitCode::SUCCESS)
}

/// Resolves on Ctrl-C (SIGINT/SIGTERM on Unix) or when `stop` drops the stop
/// file, which it consumes.
async fn stop_requested(stopfile: &Path) {
    let signal = crate::shutdown::wait_for_signal();
    tokio::pin!(signal);
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = &mut signal => return,
            _ = tick.tick() => {
                if std::fs::remove_file(stopfile).is_ok() {
                    return;
                }
            }
        }
    }
}

/// Launch `wingman indexd` detached from this terminal, then wait briefly for
/// it to claim the pidfile so a child that dies on startup is reported.
async fn start(paths: &ProjectPaths) -> Result<ExitCode> {
    use std::process::{Command, Stdio};

    if let Some(pid) = live_pid(&paths.dir) {
        println!("indexd: already running (pid {pid})");
        return Ok(ExitCode::SUCCESS);
    }
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("creating {}", paths.dir.display()))?;
    let log_path = paths.dir.join("indexd.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening log {}", log_path.display()))?;

    let exe = std::env::current_exe().context("resolving current executable")?;
    let mut cmd = Command::new(exe);
    cmd.arg("indexd")
        .current_dir(&paths.root)
        .stdin(Stdio::null())
        .stdout(log.try_clone().context("cloning log handle")?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe; runs post-fork / pre-exec.
        unsafe {
            cmd.pre_exec(|| {
                let _ = nix::unistd::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS (0x8) | CREATE_NEW_PROCESS_GROUP (0x200): no console,
        // not part of this shell's Ctrl+C group.
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let mut child = cmd.spawn().context("spawning indexd")?;
    let pid = child.id();

    let deadline = Instant::now() + START_WAIT;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            eprintln!(
                "wingman: indexd exited during startup ({status}); see {}",
                log_path.display()
            );
            return Ok(ExitCode::FAILURE);
        }
        if read_pid(&pidfile(&paths.dir)) == Some(pid) {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    println!("indexd: started (pid {pid}, log: {})", log_path.display());
    Ok(ExitCode::SUCCESS)
}

async fn stop(dir: &Path) -> Result<ExitCode> {
    let Some(pid) = live_pid(dir) else {
        println!("indexd: not running");
        return Ok(ExitCode::SUCCESS);
    };
    std::fs::write(dir.join("indexd.stop"), "").context("writing stop request")?;
    let deadline = Instant::now() + STOP_WAIT;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            println!("indexd: stopped (pid {pid})");
            return Ok(ExitCode::SUCCESS);
        }
        tokio::time::sleep(POLL).await;
    }
    // The request stays in place: a daemon still loading its embedder reads
    // it as soon as it gets going.
    eprintln!(
        "wingman: asked indexd (pid {pid}) to stop, but it is still running after {}s. \
         If that pid is not indexd, delete {}.",
        STOP_WAIT.as_secs(),
        pidfile(dir).display()
    );
    Ok(ExitCode::FAILURE)
}

fn report_status(paths: &ProjectPaths) -> Result<ExitCode> {
    match live_pid(&paths.dir) {
        Some(pid) => println!("indexd: running (pid {pid})"),
        None => println!("indexd: not running (start with `wingman indexd start`)"),
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

fn pidfile(dir: &Path) -> PathBuf {
    dir.join("indexd.pid")
}

/// Removes the pidfile when the daemon that claimed it exits.
struct PidGuard(PathBuf);

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Take `.wingman/indexd.pid` for this process. `Ok(Err(pid))` when a live
/// daemon already holds it; a stale one is cleared and the claim retried.
fn claim_pidfile(dir: &Path) -> Result<std::result::Result<PidGuard, u32>> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = pidfile(dir);
    for _ in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                let guard = PidGuard(path.clone());
                write!(f, "{}", std::process::id())?;
                return Ok(Ok(guard));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Some(pid) = live_pid(dir) {
                    return Ok(Err(pid));
                }
            }
            Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }
    Err(anyhow!(
        "could not claim {} (another indexd is starting?)",
        path.display()
    ))
}

fn read_pid(pidfile: &Path) -> Option<u32> {
    std::fs::read_to_string(pidfile).ok()?.trim().parse().ok()
}

/// The pid of this project's running indexd, given its `.wingman` directory.
/// A pidfile naming a dead process is deleted, as is an unreadable one old
/// enough that it is not a claim still being written.
pub fn live_pid(dir: &Path) -> Option<u32> {
    let path = pidfile(dir);
    match read_pid(&path) {
        Some(pid) if process_alive(pid) => return Some(pid),
        Some(_) => {}
        None => {
            let age = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())?;
            if age < Duration::from_secs(10) {
                return None;
            }
        }
    }
    let _ = std::fs::remove_file(&path);
    None
}

#[cfg(unix)]
pub(crate) fn process_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    // Pid 0 and negative pids address process groups, not a process.
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    if raw <= 0 {
        return false;
    }
    // Signal 0 checks existence without delivering anything; EPERM means it
    // exists but belongs to someone else.
    matches!(kill(Pid::from_raw(raw), None), Ok(()) | Err(Errno::EPERM))
}

#[cfg(windows)]
pub(crate) fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return false;
    }
    // SAFETY: plain Win32 calls; the handle is checked before use and closed.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // Access denied means the process exists but is not ours to open.
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code) != 0;
        CloseHandle(handle);
        // An exited process still opens while anything holds a handle to it;
        // only STILL_ACTIVE means running.
        ok && code == STILL_ACTIVE as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pid that was alive a moment ago and is not any more.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        drop(child);
        pid
    }

    #[test]
    fn this_process_is_alive_and_an_exited_child_is_not() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(dead_pid()));
        assert!(!process_alive(0));
        assert!(!process_alive(u32::MAX));
    }

    #[test]
    fn stale_pidfile_is_cleared_and_live_one_kept() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(pidfile(dir.path()), dead_pid().to_string()).unwrap();
        assert_eq!(live_pid(dir.path()), None);
        assert!(!pidfile(dir.path()).exists(), "stale pidfile removed");

        std::fs::write(pidfile(dir.path()), std::process::id().to_string()).unwrap();
        assert_eq!(live_pid(dir.path()), Some(std::process::id()));
        assert!(pidfile(dir.path()).exists());
    }

    #[test]
    fn fresh_unparseable_pidfile_is_left_for_its_writer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(pidfile(dir.path()), "").unwrap();
        assert_eq!(live_pid(dir.path()), None);
        assert!(pidfile(dir.path()).exists());
    }

    #[test]
    fn claim_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let Ok(guard) = claim_pidfile(dir.path()).unwrap() else {
            panic!("first claim refused");
        };
        assert_eq!(read_pid(&pidfile(dir.path())), Some(std::process::id()));
        // This process holds it and is alive, so a second claim is refused.
        assert_eq!(
            claim_pidfile(dir.path()).unwrap().err(),
            Some(std::process::id())
        );
        drop(guard);
        assert!(!pidfile(dir.path()).exists());
    }

    #[test]
    fn claim_takes_over_a_stale_pidfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(pidfile(dir.path()), dead_pid().to_string()).unwrap();
        let guard = claim_pidfile(dir.path()).unwrap();
        assert!(guard.is_ok());
        assert_eq!(read_pid(&pidfile(dir.path())), Some(std::process::id()));
    }

    /// The daemon claims the pidfile before opening the index, so a mismatch
    /// guard that only asked "is indexd live?" made a restarted daemon refuse
    /// the very rebuild its restart was for.
    #[test]
    fn the_daemon_itself_rebuilds_an_index_from_another_embedder() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::from_root(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.dir).unwrap();
        drop(wingman_rag::IndexStore::open(&paths.index_db, "some-other-embedder", 3).unwrap());
        let Ok(_guard) = claim_pidfile(&paths.dir).unwrap() else {
            panic!("claim refused");
        };
        assert!(runtime::build_indexer(&paths).unwrap().is_some());
    }

    #[tokio::test]
    async fn stop_request_file_is_consumed() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("indexd.stop");
        std::fs::write(&stop, "").unwrap();
        tokio::time::timeout(Duration::from_secs(5), stop_requested(&stop))
            .await
            .expect("stop file honoured");
        assert!(!stop.exists());
    }
}
