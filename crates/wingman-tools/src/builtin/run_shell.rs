//! `run_shell`: execute a shell command, capturing combined output.
//!
//! Uses `cmd.exe /C` on Windows and `sh -c` elsewhere. Output is captured
//! with a hard 60s timeout; stderr is appended after stdout under a marker
//! so the model can tell them apart.

use crate::{Tool, ToolCtx};
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

#[async_trait]
impl Tool for RunShell {
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

        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd.exe");
            c.arg("/C").arg(&args.command);
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c").arg(&args.command);
            c
        };
        cmd.current_dir(&cwd);

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
