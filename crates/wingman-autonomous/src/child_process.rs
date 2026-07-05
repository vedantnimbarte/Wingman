//! Cross-platform child-process supervision.
//!
//! Per `plan.md` § Phase 3 the worker subprocess must be killable cleanly,
//! including any subprocess tree the worker spawns (e.g. `run_shell` → cargo
//! → rustc). Naively dropping a `tokio::process::Child` reaps the immediate
//! child but leaves grandchildren orphaned.
//!
//! - **Unix** — spawn the child in its own process group (`setsid` /
//!   `setpgid` via `pre_exec`) and signal the whole group with
//!   `kill(-pgid, SIGTERM)` then `SIGKILL` after a grace period.
//! - **Windows** — assign the child to a Job Object created with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Closing the job handle (or
//!   calling `TerminateJobObject`) reaps the whole process tree. Fall back
//!   to `taskkill /T /F /PID <pid>` if the Job-Object path errors out.
//!
//! Callers see one trait — [`Supervisor`] — that hides the platform fork.

use std::process::Stdio;
use std::time::Duration;

use thiserror::Error;
use tokio::process::{Child, Command};

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("child process {pid} is no longer running")]
    NotRunning { pid: u32 },
    #[error("platform error: {0}")]
    Platform(String),
}

/// Builder for spawning a supervised child.
pub struct SupervisedCommand {
    cmd: Command,
}

impl SupervisedCommand {
    /// Wrap a [`tokio::process::Command`] for supervised spawn. Stdio is
    /// pre-configured to capture stdout (NDJSON event stream) and stderr
    /// (diagnostics); the caller may override before [`spawn`].
    pub fn new<S: AsRef<std::ffi::OsStr>>(program: S) -> Self {
        let mut cmd = Command::new(program);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self { cmd }
    }

    /// Access the underlying tokio Command for further configuration
    /// (arg(), env(), current_dir(), …).
    pub fn command_mut(&mut self) -> &mut Command {
        &mut self.cmd
    }

    /// Spawn the configured command under supervision.
    ///
    /// On Unix the child is placed in its own process group so the
    /// supervisor can kill the whole tree later. On Windows the child is
    /// assigned to a fresh Job Object with `KILL_ON_JOB_CLOSE`; if Job
    /// Object setup fails we still return a supervisor that falls back to
    /// `taskkill /T /F` for the kill path.
    pub fn spawn(mut self) -> Result<Supervisor, SupervisorError> {
        #[cfg(unix)]
        {
            // SAFETY: setsid is async-signal-safe and is the standard
            // mechanism for putting a child in its own process group. The
            // closure runs post-fork / pre-exec in the child.
            // Note: tokio::process::Command exposes `pre_exec` as an
            // inherent method on Unix, so no `CommandExt` import is needed.
            unsafe {
                self.cmd.pre_exec(|| match nix::unistd::setsid() {
                    Ok(_) => Ok(()),
                    // EPERM happens when we're already a session leader,
                    // which is harmless here.
                    Err(nix::errno::Errno::EPERM) => Ok(()),
                    Err(e) => Err(std::io::Error::other(e)),
                });
            }
        }

        let child = self.cmd.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| SupervisorError::Platform("spawned child has no pid".into()))?;

        #[cfg(windows)]
        let job = match windows_impl::assign_to_new_job(pid) {
            Ok(handle) => Some(handle),
            Err(e) => {
                tracing::warn!(
                    pid,
                    error = %e,
                    "Job Object setup failed; will fall back to taskkill for tree-kill",
                );
                None
            }
        };

        Ok(Supervisor {
            child: Some(child),
            pid,
            #[cfg(windows)]
            job,
        })
    }
}

/// A supervised running child. Drop the supervisor to terminate the whole
/// process tree.
pub struct Supervisor {
    child: Option<Child>,
    pid: u32,

    #[cfg(windows)]
    job: Option<windows_impl::JobHandle>,
}

impl Supervisor {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Borrow the underlying child for waiting / streaming I/O.
    pub fn child_mut(&mut self) -> Option<&mut Child> {
        self.child.as_mut()
    }

    /// Take ownership of the child. Used when the caller wants to hand the
    /// stdio handles off to a parser task — the [`Supervisor`] still owns
    /// the platform kill handle (Unix pgid / Windows Job) and can terminate
    /// the tree even after the `Child` has been moved out.
    pub fn take_child(&mut self) -> Option<Child> {
        self.child.take()
    }

    /// Terminate the entire process tree. Sends SIGTERM (Unix) /
    /// `TerminateJobObject` (Windows) first; falls through to SIGKILL /
    /// `taskkill /T /F` after `grace` if the child is still alive. Returns
    /// once the immediate child has been reaped (or `grace` has elapsed).
    pub async fn terminate(&mut self, grace: Duration) -> Result<(), SupervisorError> {
        self.signal_terminate()?;
        // Best-effort grace period.
        let still_alive = if let Some(child) = self.child.as_mut() {
            match tokio::time::timeout(grace, child.wait()).await {
                Ok(Ok(_status)) => false,
                Ok(Err(e)) => return Err(SupervisorError::Io(e)),
                Err(_elapsed) => true,
            }
        } else {
            false
        };
        if still_alive {
            self.signal_kill()?;
            if let Some(child) = self.child.as_mut() {
                let _ = child.wait().await;
            }
        }
        Ok(())
    }

    /// Send the "polite" termination signal (SIGTERM / TerminateJobObject).
    pub fn signal_terminate(&self) -> Result<(), SupervisorError> {
        #[cfg(unix)]
        {
            unix_impl::signal_group(self.pid, nix::sys::signal::Signal::SIGTERM)?;
            Ok(())
        }
        #[cfg(windows)]
        {
            if let Some(job) = self.job.as_ref() {
                windows_impl::terminate_job(job)?;
                return Ok(());
            }
            windows_impl::taskkill_tree(self.pid)?;
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(SupervisorError::Platform(
                "unsupported platform for tree-kill".into(),
            ))
        }
    }

    /// Send the "force" termination signal (SIGKILL / taskkill /F).
    pub fn signal_kill(&self) -> Result<(), SupervisorError> {
        #[cfg(unix)]
        {
            unix_impl::signal_group(self.pid, nix::sys::signal::Signal::SIGKILL)?;
            Ok(())
        }
        #[cfg(windows)]
        {
            // TerminateJobObject is already a hard kill; only fall back to
            // taskkill /F when the job handle is gone.
            if let Some(job) = self.job.as_ref() {
                windows_impl::terminate_job(job)?;
                return Ok(());
            }
            windows_impl::taskkill_tree_force(self.pid)?;
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(SupervisorError::Platform(
                "unsupported platform for tree-kill".into(),
            ))
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        // Best-effort: kill the tree synchronously. We don't have an async
        // context in Drop; the OS will reap stragglers via the Job Object
        // / pgid signal regardless.
        let _ = self.signal_kill();
    }
}

#[cfg(unix)]
mod unix_impl {
    use super::SupervisorError;
    use nix::errno::Errno;
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    pub fn signal_group(pid: u32, sig: Signal) -> Result<(), SupervisorError> {
        // Negative pid → "send to the process group whose pgid is |pid|".
        // Children spawned with setsid() have their own group whose pgid
        // equals their pid.
        let pgid = Pid::from_raw(-(pid as i32));
        match kill(pgid, sig) {
            Ok(()) => Ok(()),
            // ESRCH = no such process group; treat as "already gone".
            Err(Errno::ESRCH) => Ok(()),
            Err(e) => Err(SupervisorError::Platform(e.to_string())),
        }
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::SupervisorError;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// RAII wrapper around a Job Object handle; closes on drop.
    pub struct JobHandle(HANDLE);

    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    impl JobHandle {
        pub fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for JobHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                // CloseHandle on a job created with KILL_ON_JOB_CLOSE
                // terminates every assigned process — exactly what we want
                // for orphan cleanup.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    pub fn assign_to_new_job(pid: u32) -> Result<JobHandle, SupervisorError> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(io_err("CreateJobObjectW"));
            }
            let handle = JobHandle(job);

            // Configure KILL_ON_JOB_CLOSE so dropping the handle reaps the
            // tree without an explicit Terminate call.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation = JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..std::mem::zeroed()
            };
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                return Err(io_err("SetInformationJobObject"));
            }

            let process = OpenProcess(PROCESS_TERMINATE | PROCESS_SET_QUOTA, 0, pid);
            if process.is_null() {
                return Err(io_err("OpenProcess"));
            }
            let assigned = AssignProcessToJobObject(job, process);
            CloseHandle(process);
            if assigned == 0 {
                return Err(io_err("AssignProcessToJobObject"));
            }

            Ok(handle)
        }
    }

    pub fn terminate_job(job: &JobHandle) -> Result<(), SupervisorError> {
        unsafe {
            if TerminateJobObject(job.raw(), 1) == 0 {
                return Err(io_err("TerminateJobObject"));
            }
        }
        Ok(())
    }

    /// Fallback path when Job Object setup failed: spawn `taskkill /T /F`
    /// to walk the process tree by pid.
    pub fn taskkill_tree(pid: u32) -> Result<(), SupervisorError> {
        run_taskkill(pid, false)
    }

    pub fn taskkill_tree_force(pid: u32) -> Result<(), SupervisorError> {
        run_taskkill(pid, true)
    }

    fn run_taskkill(pid: u32, force: bool) -> Result<(), SupervisorError> {
        let mut cmd = std::process::Command::new("taskkill");
        cmd.arg("/T").arg("/PID").arg(pid.to_string());
        if force {
            cmd.arg("/F");
        }
        match cmd.status() {
            Ok(_) => Ok(()), // Non-zero exit usually means the process is
            // already gone; treat as success.
            Err(e) => Err(SupervisorError::Io(e)),
        }
    }

    fn io_err(label: &str) -> SupervisorError {
        SupervisorError::Io(std::io::Error::other(format!(
            "{label} failed: {}",
            std::io::Error::last_os_error()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Smoke test: spawn a long-running OS command and tree-kill it. Uses
    /// `cmd /c ping` on Windows (always present) and `sleep` on Unix.
    #[tokio::test]
    async fn spawn_and_terminate_long_running_command() {
        #[cfg(windows)]
        let mut sup = {
            let mut sc = SupervisedCommand::new("cmd");
            sc.command_mut()
                .arg("/c")
                .arg("ping")
                .arg("-n")
                .arg("30")
                .arg("127.0.0.1");
            sc.spawn().expect("spawn")
        };

        #[cfg(unix)]
        let mut sup = {
            let mut sc = SupervisedCommand::new("sleep");
            sc.command_mut().arg("30");
            sc.spawn().expect("spawn")
        };

        assert!(sup.pid() > 0);
        sup.terminate(Duration::from_secs(2))
            .await
            .expect("terminate");
    }
}
