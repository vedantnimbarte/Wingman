//! `run_shell`: execute a shell command, capturing combined output.
//!
//! Uses `cmd.exe /C` on Windows and `sh -c` elsewhere. Output is captured
//! with a hard 60s timeout; stderr is appended after stdout under a marker
//! so the model can tell them apart.

use crate::{Capability, Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::Command;
use wingman_core::{ToolOutcome, ToolSpec};

pub struct RunShell;

#[derive(Debug, Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Start it and return a job id instead of waiting.
    #[serde(default)]
    background: bool,
}

/// Say once per process that shell commands are running unconfined, so the
/// user knows which guarantee they do *not* have. Once, because run_shell is
/// called constantly and a per-call warning would be noise.
fn warn_unconfined_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            target: "wingman::sandbox",
            "shell commands run unconfined: no sandbox mechanism found. Install bubblewrap (Linux) for filesystem containment, or set [tools].shell_sandbox = \"required\" to refuse instead."
        );
    });
}

/// Windows only: the Job Object contains the process but not its file
/// access, so say once that the filesystem half of the guarantee is missing.
#[cfg(windows)]
fn warn_no_path_scoping_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            target: "wingman::sandbox",
            "shell commands run in a Job Object (no orphans, no clipboard or cross-process handles, capped process count) but their filesystem access is NOT confined — a command can still read credentials outside the project. See issue #124."
        );
    });
}

#[async_trait]
impl Tool for RunShell {
    fn capabilities(&self) -> Capability {
        Capability::SHELL
    }

    /// `run_shell` bounds itself — `timeout_secs` (default 60, max 600), and
    /// on timeout it kills the whole process tree rather than orphaning it.
    /// The registry backstop is shorter than that ceiling, so letting it
    /// apply would cap a legitimate long build at the default.
    fn owns_timeout(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_shell".into(),
            description: "Execute a shell command and return its combined stdout/stderr. Times \
                          out after 60s by default (max 600). Set `background: true` to start it \
                          and get a job id back instead of waiting - then use job_output, \
                          job_stop and job_list."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string", "description": "Working directory; defaults to project root." },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600 },
                    "background": { "type": "boolean", "default": false, "description": "Start the command and return a job id immediately instead of waiting. Use for dev servers, watch processes, and builds longer than the 600s ceiling; collect output with job_output." }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let (cmd, policy) = match prepare(&args, ctx) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::err(e),
        };
        if args.background {
            // Same prepared command, so the sandbox policy, denylist and
            // env scrub applied identically - a background command is not
            // a less-guarded one.
            let supervised = crate::child_process::SupervisedCommand::from_command(cmd);
            return match ctx.jobs.start(&args.command, supervised) {
                Ok(id) => ToolOutcome::ok(format!(
                    "started {id}\nCollect output with job_output(id: \"{id}\"); stop it with \
                     job_stop. It is killed with its whole process tree when the session ends."
                )),
                Err(e) => ToolOutcome::err(e),
            };
        }
        let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(60).min(600));
        let output = match run_captured(cmd, timeout, &policy).await {
            Ok(o) => o,
            Err(e) => return ToolOutcome::err(e),
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut body = String::new();
        body.push_str(&format!(
            "[exit: {}]\n",
            output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into())
        ));
        if !stdout.is_empty() {
            body.push_str("[stdout]\n");
            body.push_str(&stdout);
            if !stdout.ends_with('\n') {
                body.push('\n');
            }
        }
        if !stderr.is_empty() {
            body.push_str("[stderr]\n");
            body.push_str(&stderr);
        }
        if output.status.success() {
            ToolOutcome::ok(body)
        } else {
            ToolOutcome::err(body)
        }
    }
}

/// Everything that has to happen before a shell command may run:
/// permission mode, the project denylist, cwd resolution, OS-level
/// containment, and scrubbing credentials out of the child's environment.
///
/// Extracted so the foreground and background paths cannot drift apart.
/// A second copy of this is a second place for the sandbox policy or the
/// denylist to be subtly wrong, and only one of them would be tested.
///
/// Returns the configured command and the resolved sandbox policy.
fn prepare(args: &Args, ctx: &ToolCtx) -> Result<(Command, String), String> {
    if !ctx.allows_shell() {
        return Err(format!("shell denied under permission mode {}", ctx.mode()));
    }
    if ctx.is_shell_denied(&args.command) {
        return Err(format!(
            "shell command denied by project denylist: {}",
            args.command
        ));
    }
    let cwd = args
        .cwd
        .as_deref()
        .map(|p| ctx.resolve(p))
        .unwrap_or_else(|| ctx.project_root.clone());

    // OS-level containment. The permission modes confine the *file
    // tools* to the project tree, but a shell command can otherwise read
    // anything the user can — so `cat ~/.ssh/id_rsa` succeeded in the very
    // mode where `read_file` on that path was refused. See `crate::sandbox`.
    let policy = ctx.shell_sandbox.as_str();
    let sandboxed = if policy == "off" {
        None
    } else {
        crate::sandbox::wrap(&args.command, &ctx.project_root, &std::env::temp_dir())
    };

    // `required` means "the filesystem is confined", so it gates on
    // `scopes_filesystem` rather than on any mechanism being present:
    // the Windows Job Object is real containment but not *that* one, and
    // accepting it here would silently weaken an opt-in.
    if policy == "required" && !crate::sandbox::availability().scopes_filesystem() {
        return Err(format!(
            "refusing to run: [tools].shell_sandbox is `required` but no filesystem-scoping sandbox is available on this machine ({}). Install bubblewrap (Linux), use macOS, or set `shell_sandbox = \"auto\"` to accept weaker containment.",
            crate::sandbox::availability().label()
        ));
    }

    let mut cmd = match &sandboxed {
        Some(argv) => {
            let mut c = Command::new(&argv[0]);
            c.args(&argv[1..]);
            c
        }
        None => {
            if policy == "auto" {
                warn_unconfined_once();
            }
            if cfg!(windows) {
                let mut c = Command::new("cmd.exe");
                c.arg("/C").arg(&args.command);
                c
            } else {
                let mut c = Command::new("sh");
                c.arg("-c").arg(&args.command);
                c
            }
        }
    };
    cmd.current_dir(&cwd);
    // Don't hand the child our API keys. It has no need for them, and a
    // shell command is the easiest place for an injected instruction to
    // read one out of the environment and send it somewhere.
    for (k, _) in std::env::vars() {
        let upper = k.to_ascii_uppercase();
        if upper.ends_with("_API_KEY")
            || upper.ends_with("_TOKEN")
            || upper.starts_with("AWS_")
            || upper == "GITHUB_TOKEN"
        {
            cmd.env_remove(k);
        }
    }

    Ok((cmd, policy.to_string()))
}

/// Spawn, capture, and time out — with whatever post-spawn containment the
/// platform offers.
///
/// On Windows the child goes into a Job Object (see
/// [`crate::sandbox::windows_job`]); the guard is held for the life of the
/// command, so a timeout drops it and the whole tree dies instead of being
/// orphaned. Everywhere else this is `cmd.output()` with a timeout, exactly
/// as before — the confinement there is already in the argv.
async fn run_captured(
    #[allow(unused_mut)] mut cmd: Command,
    timeout: Duration,
    #[allow(unused_variables)] policy: &str,
) -> Result<std::process::Output, String> {
    #[cfg(windows)]
    {
        use std::process::Stdio;
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
        // Assign before the command has had a chance to do much. `None` means
        // the child already exited, which is nothing to contain.
        let _job = child.id().and_then(|pid| {
            match crate::sandbox::windows_job::confine(pid) {
                Ok(g) => {
                    if policy != "off" {
                        warn_no_path_scoping_once();
                    }
                    Some(g)
                }
                Err(e) => {
                    // Containment failed; the command still runs, but say so
                    // rather than implying a guarantee that isn't there.
                    tracing::warn!(
                        target: "wingman::sandbox",
                        error = %e,
                        "Job Object containment failed; shell command runs unconfined"
                    );
                    None
                }
            }
        });
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => Ok(o),
            Ok(Err(e)) => Err(format!("spawn failed: {e}")),
            // Dropping `_job` here kills the tree.
            Err(_) => Err(format!("timed out after {}s", timeout.as_secs())),
        }
    }
    #[cfg(not(windows))]
    {
        match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(o)) => Ok(o),
            Ok(Err(e)) => Err(format!("spawn failed: {e}")),
            Err(_) => Err(format!("timed out after {}s", timeout.as_secs())),
        }
    }
}
