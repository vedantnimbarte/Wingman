//! Background shell jobs.
//!
//! `run_shell` blocks the turn and is capped at 600 seconds, which rules out
//! dev servers, watch processes, long test suites, and cold builds of a large
//! workspace. A job is the same command, started and left running: the tool
//! returns an id immediately and the agent collects output when it wants it.
//!
//! Deliberately a process table and three tools rather than a capability
//! seam — see [decision 0008](../../../docs/decisions/0008-defer-background-jobs-and-pty.md).
//!
//! Two properties this has to hold, both learned elsewhere in this codebase:
//!
//! - **Output is bounded.** A dev server emits output forever. Buffering it
//!   all is the same unbounded-growth mistake `@file` attachments and tool
//!   output each had to fix, and here it would be a leak that runs for hours.
//!   The buffer keeps the most recent bytes, because for a build or a server
//!   the interesting part is the end.
//! - **Stopping means the whole tree.** Killing `sh` leaves `npm`, and `npm`
//!   leaves `node`. [`crate::child_process`] already solves this on both
//!   platforms, so jobs use it rather than growing a second answer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::child_process::{SupervisedCommand, Supervisor};

/// Most output bytes retained per job.
///
/// The tail, not the head: a build's errors and a server's latest request are
/// both at the end. What falls off the front is counted and reported, so the
/// agent is never quietly shown a partial picture as if it were whole.
const MAX_BUFFERED_BYTES: usize = 128 * 1024;

/// What a job is doing now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Running,
    /// Exited on its own. `None` when the platform reported no code (killed
    /// by a signal, typically).
    Exited(Option<i32>),
    /// Stopped by `job_stop`, or by the table being dropped at session end.
    Killed,
}

impl JobState {
    pub fn label(&self) -> String {
        match self {
            Self::Running => "running".into(),
            Self::Exited(Some(c)) => format!("exited({c})"),
            Self::Exited(None) => "exited(?)".into(),
            Self::Killed => "killed".into(),
        }
    }
}

/// One background command.
struct Job {
    id: String,
    command: String,
    state: Arc<Mutex<JobState>>,
    /// Most recent output, capped at [`MAX_BUFFERED_BYTES`].
    output: Arc<Mutex<Tail>>,
    /// Dropping this kills the process tree.
    supervisor: Mutex<Option<Supervisor>>,
    started: std::time::Instant,
}

/// A bounded, tail-biased byte buffer.
#[derive(Default)]
pub struct Tail {
    buf: Vec<u8>,
    /// Bytes discarded off the front, so a reader can be told what it missed
    /// rather than shown a truncated log that looks complete.
    dropped: usize,
}

impl Tail {
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > MAX_BUFFERED_BYTES {
            let excess = self.buf.len() - MAX_BUFFERED_BYTES;
            self.buf.drain(..excess);
            self.dropped += excess;
        }
    }

    fn render(&self) -> String {
        let text = String::from_utf8_lossy(&self.buf).into_owned();
        if self.dropped == 0 {
            text
        } else {
            format!(
                "… [wingman] {} earlier bytes dropped; this is the most recent \
                 {} KiB …\n{text}",
                self.dropped,
                MAX_BUFFERED_BYTES / 1024
            )
        }
    }
}

/// The session's background jobs.
///
/// Shared through [`crate::ToolCtx`], so a subagent sees the same table as its
/// parent: a job started by delegated work still belongs to the session, and
/// giving children their own table would orphan whatever they left running.
#[derive(Default)]
// Deliberately not Debug-printing the table: a job's buffered output can
// be 128 KiB, and a ToolCtx is logged in places where that would be noise.
#[allow(missing_debug_implementations)]
pub struct JobTable {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    next: AtomicU64,
}

impl JobTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start `cmd` in the background and return its job id.
    ///
    /// `cmd` is expected to have come from `run_shell`'s own preparation, so
    /// the sandbox policy, denylist, and environment scrub already applied —
    /// a background command is not a less-guarded command.
    pub fn start(&self, command: &str, mut cmd: SupervisedCommand) -> Result<String, String> {
        use std::process::Stdio;
        cmd.command_mut()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        let mut supervisor = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
        // Taking the child is safe: the supervisor kills by process group
        // (Unix) or Job Object handle (Windows), neither of which needs the
        // `Child`. It keeps working as the tree-killer after this.
        let mut child = supervisor
            .take_child()
            .ok_or_else(|| "child vanished immediately after spawn".to_string())?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let id = format!("job-{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
        let state = Arc::new(Mutex::new(JobState::Running));
        let output = Arc::new(Mutex::new(Tail::default()));

        // Drain both pipes continuously. Without this the child blocks once
        // the pipe buffer fills — a "background" job that silently wedges
        // after a few KiB would be worse than not having the feature.
        if let Some(mut r) = stdout {
            let sink = output.clone();
            tokio::spawn(async move { drain(&mut r, sink).await });
        }
        if let Some(mut r) = stderr {
            let sink = output.clone();
            tokio::spawn(async move { drain(&mut r, sink).await });
        }

        // Reap it, so `job_list` reports a finished job as finished rather
        // than leaving it "running" forever.
        {
            let state = state.clone();
            tokio::spawn(async move {
                let code = child.wait().await.ok().and_then(|s| s.code());
                let mut st = state.lock().unwrap();
                // A stop already set the state; do not overwrite it with the
                // exit status the kill itself produced.
                if *st == JobState::Running {
                    *st = JobState::Exited(code);
                }
            });
        }

        let job = Arc::new(Job {
            id: id.clone(),
            command: command.to_string(),
            state,
            output,
            supervisor: Mutex::new(Some(supervisor)),
            started: std::time::Instant::now(),
        });
        self.jobs.lock().unwrap().insert(id.clone(), job);
        Ok(id)
    }

    /// Output so far, plus the job's state.
    pub fn output(&self, id: &str) -> Option<(String, JobState)> {
        let job = self.jobs.lock().unwrap().get(id).cloned()?;
        let text = job.output.lock().unwrap().render();
        let state = job.state.lock().unwrap().clone();
        Some((text, state))
    }

    /// Stop a job and its whole process tree.
    pub fn stop(&self, id: &str) -> Option<JobState> {
        let job = self.jobs.lock().unwrap().get(id).cloned()?;
        *job.state.lock().unwrap() = JobState::Killed;
        if let Some(sup) = job.supervisor.lock().unwrap().take() {
            // Dropping the supervisor signals the group / closes the job
            // object, which is what actually reaps the grandchildren.
            drop(sup);
        }
        Some(JobState::Killed)
    }

    /// One line per job: id, state, how long it has run, and the command.
    pub fn list(&self) -> Vec<String> {
        let jobs = self.jobs.lock().unwrap();
        let mut rows: Vec<(String, String)> = jobs
            .values()
            .map(|j| {
                let state = j.state.lock().unwrap().label();
                let secs = j.started.elapsed().as_secs();
                (
                    j.id.clone(),
                    format!("{:<10} {:<12} {:>4}s  {}", j.id, state, secs, j.command),
                )
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows.into_iter().map(|(_, line)| line).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.lock().unwrap().is_empty()
    }
}

/// Copy a pipe into the job's buffer until it closes.
///
/// Generic over the two pipe types so stdout and stderr share one path;
/// they are interleaved into a single buffer because that is the order the
/// process actually produced them in, which is what a reader wants.
async fn drain<R>(reader: &mut R, sink: Arc<Mutex<Tail>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 8192];
    while let Ok(n) = reader.read(&mut buf).await {
        if n == 0 {
            break;
        }
        sink.lock().unwrap().push(&buf[..n]);
    }
}

impl Drop for JobTable {
    /// A session ending must not leave a dev server running.
    fn drop(&mut self) {
        let jobs = std::mem::take(&mut *self.jobs.lock().unwrap());
        for (_, job) in jobs {
            if let Some(sup) = job.supervisor.lock().unwrap().take() {
                drop(sup);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_keeps_the_end_and_reports_what_it_dropped() {
        let mut tail = Tail::default();
        tail.push(&vec![b'a'; MAX_BUFFERED_BYTES]);
        tail.push(b"THE-END");
        let rendered = tail.render();
        assert!(rendered.ends_with("THE-END"), "the tail must survive");
        assert!(
            rendered.contains("earlier bytes dropped"),
            "a truncated log must not look complete"
        );
        assert_eq!(tail.dropped, 7);
    }

    #[test]
    fn a_short_tail_is_returned_verbatim() {
        let mut tail = Tail::default();
        tail.push(b"hello\n");
        assert_eq!(tail.render(), "hello\n");
    }

    #[test]
    fn state_labels_read_plainly() {
        assert_eq!(JobState::Running.label(), "running");
        assert_eq!(JobState::Exited(Some(0)).label(), "exited(0)");
        assert_eq!(JobState::Exited(None).label(), "exited(?)");
        assert_eq!(JobState::Killed.label(), "killed");
    }

    #[test]
    fn an_unknown_job_is_none_rather_than_a_panic() {
        let table = JobTable::new();
        assert!(table.output("job-99").is_none());
        assert!(table.stop("job-99").is_none());
        assert!(table.list().is_empty());
    }
}
