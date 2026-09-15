//! `checkpoint`: worker-only tool that commits the worktree's current state
//! onto the task branch (E11).
//!
//! The worker prompt mandates it before a second file is edited and after each
//! time acceptance goes green. Calling it is also what the E11 Review gate
//! looks for: the supervisor reads the attempt's recorded tool calls
//! ([`crate::checkpoint::verify`]) and fails multi-file work that never called
//! it.

use async_trait::async_trait;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_tools::{Capability, Tool, ToolCtx};

pub struct Checkpoint;

#[async_trait]
impl Tool for Checkpoint {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "checkpoint".into(),
            description: "Commit every change in this worktree onto the task branch, so a bad \
                 edit can be undone with git. Call it before you edit a second file, and again \
                 each time `run_acceptance` comes back green. Multi-file work that never \
                 checkpointed is failed when it reaches review."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "label": {
                        "type": "string",
                        "description": "What the checkpoint captures, e.g. \"parser compiles\"."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    /// Writes git objects and the branch ref by running `git`.
    fn capabilities(&self) -> Capability {
        Capability::WRITE | Capability::SHELL
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let label = args
            .get("label")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or("checkpoint");
        if !ctx.allows_write(&ctx.cwd) {
            return ToolOutcome::err(format!(
                "checkpoint refused: {} is outside the writable project",
                ctx.cwd.display()
            ));
        }
        let message = format!("wingman checkpoint: {label}");
        let cwd = ctx.cwd.clone();
        let committed =
            tokio::task::spawn_blocking(move || crate::worktree::commit_checkpoint(&cwd, &message))
                .await;
        match committed {
            Ok(Ok(Some(sha))) => ToolOutcome::ok(format!(
                "checkpoint {} committed",
                &sha[..sha.len().min(12)]
            )),
            Ok(Ok(None)) => {
                ToolOutcome::ok("nothing to commit: the worktree already matches its last commit")
            }
            Ok(Err(e)) => ToolOutcome::err(format!("checkpoint failed: {e}")),
            Err(e) => ToolOutcome::err(format!("checkpoint failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn commits_the_worktree_but_not_wingman_bookkeeping() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        if Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["init", "-q"])
            .output()
            .is_err()
        {
            eprintln!("skipping: git not available");
            return;
        }
        git(root, &["config", "user.email", "t@t.t"]);
        git(root, &["config", "user.name", "t"]);
        std::fs::write(root.join("a.rs"), "base\n").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "base"]);

        let ctx = ToolCtx::new(
            wingman_config::PermissionMode::AutoEdit,
            root.to_path_buf(),
            root.to_path_buf(),
        );
        // A clean tree has nothing to commit.
        let out = Checkpoint.run(json!({}), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("nothing to commit"), "{}", out.content);

        std::fs::write(root.join("a.rs"), "edited\n").unwrap();
        std::fs::write(root.join("b.rs"), "new\n").unwrap();
        std::fs::create_dir_all(root.join(".wingman/pilot")).unwrap();
        std::fs::write(root.join(".wingman/pilot/task-t1.json"), "{}").unwrap();
        let out = Checkpoint.run(json!({"label": "two files"}), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            git(root, &["log", "-1", "--format=%s"]),
            "wingman checkpoint: two files"
        );
        let committed = git(root, &["show", "--name-only", "--format=", "HEAD"]);
        assert_eq!(committed, "a.rs\nb.rs");
        assert!(git(root, &["status", "--porcelain"]).contains(".wingman"));
    }
}
