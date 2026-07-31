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
}

/// Say once per process that shell commands are running unconfined, so the
/// user knows which guarantee they do *not* have. Once, because run_shell is
/// called constantly and a per-call warning would be noise.
fn warn_unconfined_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            target: "wingman::sandbox",
            "shell commands run unconfined: no sandbox mechanism found.              Install bubblewrap (Linux) for filesystem containment, or set              [tools].shell_sandbox = \"required\" to refuse instead."
        );
    });
}

#[async_trait]
impl Tool for RunShell {
    fn capabilities(&self) -> Capability {
        Capability::SHELL
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_shell".into(),
            description: "Execute a shell command and return its combined stdout/stderr. Times \
                          out after 60s by default."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string", "description": "Working directory; defaults to project root." },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600 }
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
        if !ctx.allows_shell() {
            return ToolOutcome::err(format!("shell denied under permission mode {}", ctx.mode()));
        }
        if ctx.is_shell_denied(&args.command) {
            return ToolOutcome::err(format!(
                "shell command denied by project denylist: {}",
                args.command
            ));
        }
        let cwd = args
            .cwd
            .as_deref()
            .map(|p| ctx.resolve(p))
            .unwrap_or_else(|| ctx.project_root.clone());

        let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(60).min(600));

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

        if policy == "required" && sandboxed.is_none() {
            return ToolOutcome::err(format!(
                "refusing to run: [tools].shell_sandbox is `required` but no sandbox                  mechanism is available on this machine ({}). Install bubblewrap                  (Linux), use macOS, or set `shell_sandbox = \"auto\"` to accept                  unconfined execution.",
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

        let output = match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return ToolOutcome::err(format!("spawn failed: {e}")),
            Err(_) => return ToolOutcome::err(format!("timed out after {}s", timeout.as_secs())),
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
