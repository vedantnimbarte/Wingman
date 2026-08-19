//! Windows containment for `run_shell`: a Job Object around the child.
//!
//! What this buys, concretely:
//!
//! - **No orphans.** Dropping the handle (`KILL_ON_JOB_CLOSE`) reaps the whole
//!   tree, so a timed-out command can't leave a build running. Before this,
//!   `run_shell`'s timeout dropped `cmd.exe` and left its children alive.
//! - **No clipboard or cross-process handle access.** The UI restrictions stop
//!   a shell command from reading what the user copied, or grabbing a window
//!   handle belonging to a process outside the job.
//! - **No fork bombs.** `ACTIVE_PROCESS` caps the tree.
//!
//! What it does **not** buy: filesystem scoping. See the parent module.
//!
//! Assignment happens just after spawn rather than at creation, because the
//! child is spawned through `tokio::process`. A command that detaches within
//! that window escapes the job — closing it means owning `CreateProcessW`
//! (`CREATE_SUSPENDED` + resume), which is the same rewrite that path scoping
//! needs.

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP,
    JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

/// Ceiling on processes in one shell command's tree. High enough for a
/// parallel `cargo build`, low enough that a fork bomb hits a wall.
const ACTIVE_PROCESS_LIMIT: u32 = 256;

/// Owns the Job Object. Dropping it terminates every process still assigned,
/// so hold it for as long as the command may run and drop it to kill the tree.
pub struct JobGuard(HANDLE);

// The handle is just a kernel object pointer; no thread affinity.
unsafe impl Send for JobGuard {}
unsafe impl Sync for JobGuard {}

impl Drop for JobGuard {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Put `pid` (and everything it spawns) in a fresh, restricted Job Object.
///
/// `Err` means the child is running unconfined — the caller decides whether
/// that is fatal.
pub fn confine(pid: u32) -> std::io::Result<JobGuard> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(last_error("CreateJobObjectW"));
        }
        // Guard from here on, so every early return closes the handle.
        let guard = JobGuard(job);

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation = JOBOBJECT_BASIC_LIMIT_INFORMATION {
            LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            ActiveProcessLimit: ACTIVE_PROCESS_LIMIT,
            ..std::mem::zeroed()
        };
        // No JOB_OBJECT_LIMIT_JOB_MEMORY on purpose: a committed-memory cap
        // low enough to be meaningful is also low enough to fail a large
        // link step, and OOM-ing the user's build is not containment.
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            return Err(last_error("SetInformationJobObject(limits)"));
        }

        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                | JOB_OBJECT_UILIMIT_DESKTOP
                | JOB_OBJECT_UILIMIT_EXITWINDOWS
                | JOB_OBJECT_UILIMIT_GLOBALATOMS
                | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
        };
        if SetInformationJobObject(
            job,
            JobObjectBasicUIRestrictions,
            &ui as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
        ) == 0
        {
            return Err(last_error("SetInformationJobObject(ui)"));
        }

        let process = OpenProcess(PROCESS_TERMINATE | PROCESS_SET_QUOTA, 0, pid);
        if process.is_null() {
            return Err(last_error("OpenProcess"));
        }
        let assigned = AssignProcessToJobObject(job, process);
        CloseHandle(process);
        if assigned == 0 {
            return Err(last_error("AssignProcessToJobObject"));
        }
        Ok(guard)
    }
}

fn last_error(label: &str) -> std::io::Error {
    std::io::Error::other(format!(
        "{label} failed: {}",
        std::io::Error::last_os_error()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confines_a_live_child_and_kills_it_on_drop() {
        // A child that would outlive us: ping loops for about a minute.
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "ping -n 60 127.0.0.1 > NUL"])
            .spawn()
            .expect("spawn");
        let guard = confine(child.id()).expect("job object");

        // Still running while the guard is held.
        assert!(child.try_wait().expect("try_wait").is_none());

        drop(guard);
        // KILL_ON_JOB_CLOSE reaps it: the wait returns now rather than in the
        // ~60s the ping would otherwise take. The exit code Windows stamps on
        // a job-terminated process is not worth asserting on; that it died is.
        let started = std::time::Instant::now();
        let _ = child.wait().expect("wait");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "dropping the job guard should have killed the tree immediately"
        );
    }

    #[test]
    fn confining_a_dead_pid_does_not_panic() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit 0"])
            .spawn()
            .expect("spawn");
        let pid = child.id();
        let _ = child.wait();
        // Windows can keep a dead pid resolvable briefly, so either outcome
        // is legal — what matters is that it neither panics nor hangs.
        let _ = confine(pid);
    }
}
