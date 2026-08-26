//! User-defined command tools: extend the agent with a shell command from
//! config (`[[tools.custom]]`), no recompile. The tool input JSON is passed on
//! stdin and in `$WINGMAN_TOOL_INPUT`; stdout becomes the result. Gated behind
//! the shell permission — these run arbitrary commands.

use crate::{Capability, Tool, ToolCtx};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;
use wingman_core::{ToolOutcome, ToolSpec};

/// A tool backed by a shell command defined in config.
pub struct CommandTool {
    name: String,
    description: String,
    command: String,
    timeout: Duration,
}

impl CommandTool {
    pub fn new(
        name: String,
        description: String,
        command: String,
        timeout_secs: Option<u64>,
    ) -> Self {
        Self {
            name,
            description,
            command,
            timeout: Duration::from_secs(timeout_secs.unwrap_or(30).max(1)),
        }
    }
}

#[async_trait]
impl Tool for CommandTool {
    fn capabilities(&self) -> Capability {
        Capability::SHELL
    }

    /// Each custom tool carries its own `timeout_secs` (default 30), so the
    /// user's configured bound wins over the registry backstop.
    fn owns_timeout(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: format!("{} (user-defined command tool)", self.description),
            // Free-form object: the command decides how to interpret its input.
            input_schema: json!({ "type": "object", "additionalProperties": true }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        // Custom tools run arbitrary shell — require shell permission.
        if !ctx.allows_shell() {
            return ToolOutcome::err(format!(
                "custom tool `{}` runs a shell command; not permitted in the current mode \
                 (needs auto-edit/yolo)",
                self.name
            ));
        }
        if ctx.is_shell_denied(&self.command) {
            return ToolOutcome::err(format!(
                "custom tool `{}` command is blocked by the shell denylist",
                self.name
            ));
        }

        let input = serde_json::to_string(&args).unwrap_or_default();
        let command = self.command.clone();
        let cwd = ctx.cwd.clone();
        let timeout = self.timeout;

        let fut = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            use std::process::{Command, Stdio};
            let mut cmd = if cfg!(windows) {
                let mut c = Command::new("cmd");
                c.args(["/C", &command]);
                c
            } else {
                let mut c = Command::new("sh");
                c.args(["-c", &command]);
                c
            };
            cmd.current_dir(&cwd)
                .env("WINGMAN_TOOL_INPUT", &input)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(input.as_bytes());
            }
            child.wait_with_output()
        });

        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(Ok(out))) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                if out.status.success() {
                    ToolOutcome::ok(if stdout.trim().is_empty() {
                        "(command produced no output)".to_string()
                    } else {
                        stdout.into_owned()
                    })
                } else {
                    ToolOutcome::err(format!(
                        "command failed ({}):\n{}",
                        out.status,
                        if stderr.trim().is_empty() {
                            stdout
                        } else {
                            stderr
                        }
                    ))
                }
            }
            Ok(Ok(Err(e))) => ToolOutcome::err(format!("spawn failed: {e}")),
            Ok(Err(e)) => ToolOutcome::err(format!("join failed: {e}")),
            Err(_) => ToolOutcome::err(format!(
                "custom tool `{}` timed out after {}s",
                self.name,
                timeout.as_secs()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wingman_config::PermissionMode;

    fn ctx(mode: PermissionMode) -> ToolCtx {
        let dir = std::env::temp_dir();
        ToolCtx::new(mode, dir.clone(), dir)
    }

    #[tokio::test]
    async fn runs_command_and_returns_stdout() {
        // `echo` works in both cmd.exe and sh.
        let tool = CommandTool::new("say".into(), "say hi".into(), "echo hello".into(), Some(5));
        let out = tool
            .run(serde_json::json!({}), &ctx(PermissionMode::Yolo))
            .await;
        assert!(!out.is_error, "content: {}", out.content);
        assert!(out.content.contains("hello"));
    }

    #[tokio::test]
    async fn blocked_without_shell_permission() {
        let tool = CommandTool::new("say".into(), "d".into(), "echo x".into(), Some(5));
        let out = tool
            .run(serde_json::json!({}), &ctx(PermissionMode::ReadOnly))
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("not permitted"));
    }

    #[tokio::test]
    async fn nonzero_exit_is_an_error() {
        // `exit 3` works in both cmd.exe and sh.
        let tool = CommandTool::new("fail".into(), "d".into(), "exit 3".into(), Some(5));
        let out = tool
            .run(serde_json::json!({}), &ctx(PermissionMode::Yolo))
            .await;
        assert!(out.is_error);
    }
}
