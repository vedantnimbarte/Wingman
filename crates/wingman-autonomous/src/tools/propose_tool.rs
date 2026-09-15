//! `propose_tool`: worker-only tool synthesis (J7).
//!
//! A worker that keeps reaching for a command the toolset lacks can propose
//! it as a named tool. The proposal is an ordinary custom command tool
//! ([`wingman_config::CustomToolConfig`]) written to the owning project's
//! `.wingman/tools/<name>.toml`; the shared registry builder loads it into
//! every registry built after it is approved — the next worker spawned, and
//! interactive sessions in the project.
//!
//! Approval is the trust store: a tool is approved when its exact file
//! content is recorded there, so editing the file after approval revokes it.
//! On `autopilot` in a trusted project the tool approves its own proposal;
//! everywhere else the proposal waits for `wingman pilot tools approve`
//! ([`crate::approval::tool_synthesis_tier`]).
//!
//! It cannot widen what the run may do. Proposing needs the shell permission
//! and a command the shell denylist accepts, and the resulting tool runs
//! through `run_shell`'s own guards, so it is a name for a command the worker
//! could already have run — nothing more.

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_config::{ConfigError, CustomToolConfig};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_tools::{Capability, Tool, ToolCtx};

use crate::approval::ApprovalTier;

pub struct ProposeTool {
    tier: ApprovalTier,
    /// Names already in the worker's registry, which a proposal may not take.
    taken: Vec<String>,
    /// Records approval. The trust store in production; swapped in tests so
    /// they never write the user's `~/.wingman/trusted.toml`.
    approve: fn(&Path) -> Result<String, ConfigError>,
}

impl ProposeTool {
    pub fn new(tier: ApprovalTier, taken: Vec<String>) -> Self {
        Self {
            tier,
            taken,
            approve: wingman_config::trust::trust,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    name: String,
    description: String,
    command: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for ProposeTool {
    /// Writes a file under `.wingman/tools/`, and what it writes becomes a
    /// shell command other agents will run.
    fn capabilities(&self) -> Capability {
        Capability::WRITE | Capability::SHELL
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "propose_tool".into(),
            description: "Propose a new tool for this project when you keep needing a shell \
                          command the toolset lacks (e.g. querying the dev database). It \
                          becomes a named tool for workers spawned after it is approved - not \
                          for you, in this task; keep using run_shell here. The tool runs \
                          `command` with the call's input JSON in $WINGMAN_TOOL_INPUT \
                          (%WINGMAN_TOOL_INPUT% on Windows). Propose only commands that are \
                          safe to run repeatedly."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "snake_case, [a-z][a-z0-9_]*, at most 64 bytes; must not name an existing tool." },
                    "description": { "type": "string", "description": "One line telling future agents when to call it." },
                    "command": { "type": "string", "description": "Shell command the tool runs from the project root." },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600 }
                },
                "required": ["name", "description", "command"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        // The run's ceiling: a worker that may not shell out may not mint a
        // tool that does, and a command the denylist refuses stays refused.
        if !ctx.allows_shell() {
            return ToolOutcome::err(format!(
                "propose_tool needs the shell permission; not permitted under {}",
                ctx.mode()
            ));
        }
        // Synthesized tools do not load where `run_shell` is removed, so a
        // proposal here could never be called. Say so rather than accept it.
        if !self.taken.iter().any(|t| t == "run_shell") {
            return ToolOutcome::err(
                "run_shell is disabled for this run, so a synthesized tool could never load",
            );
        }
        if ctx.is_shell_denied(&args.command) {
            return ToolOutcome::err("command is blocked by the shell denylist");
        }
        if !wingman_config::valid_synthesized_tool_name(&args.name) {
            return ToolOutcome::err(format!(
                "invalid tool name `{}`: use [a-z][a-z0-9_]*, at most 64 bytes",
                args.name
            ));
        }
        if args.name == "propose_tool" || self.taken.contains(&args.name) {
            return ToolOutcome::err(format!("a tool named `{}` already exists", args.name));
        }
        if args.description.trim().is_empty() || args.command.trim().is_empty() {
            return ToolOutcome::err("description and command must not be empty");
        }

        let tool = CustomToolConfig {
            name: args.name.clone(),
            description: args.description.trim().to_string(),
            command: args.command,
            timeout_secs: args.timeout_secs.map(|t| t.clamp(1, 600)),
        };
        let project = wingman_config::find_owning_project_root(&ctx.project_root);
        let path = match wingman_config::write_synthesized_tool(&project, &tool) {
            Ok(p) => p,
            Err(ConfigError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                return ToolOutcome::err(format!(
                    "`{}` has already been proposed for this project; pick another name",
                    tool.name
                ))
            }
            Err(e) => return ToolOutcome::err(format!("could not write the proposal: {e}")),
        };
        tracing::info!(
            target: "pilot::toolsynth",
            tool = %tool.name,
            approval = %self.tier,
            path = %path.display(),
            "tool proposed"
        );

        if self.tier == ApprovalTier::Auto {
            return match (self.approve)(&path) {
                Ok(_) => ToolOutcome::ok(format!(
                    "`{}` approved (autopilot, trusted project) and written to {}. Workers \
                     spawned from now on can call it; keep using run_shell for this task.",
                    tool.name,
                    path.display()
                )),
                Err(e) => ToolOutcome::err(format!(
                    "`{}` was written to {} but could not be approved ({e}); it stays \
                     pending until `wingman pilot tools approve {}`.",
                    tool.name,
                    path.display(),
                    tool.name
                )),
            };
        }
        ToolOutcome::ok(format!(
            "`{}` proposed at {} and waiting for approval (`wingman pilot tools approve {}`). \
             It is not callable until approved; keep using run_shell for this task.",
            tool.name,
            path.display(),
            tool.name
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wingman_config::PermissionMode;

    fn args(name: &str, command: &str) -> Value {
        json!({ "name": name, "description": "query the dev db", "command": command })
    }

    fn ctx_at(root: &Path, mode: PermissionMode, deny: Vec<String>) -> ToolCtx {
        ToolCtx::new_with_config(mode, root.to_path_buf(), root.to_path_buf(), deny, false)
    }

    fn fake_approve(path: &Path) -> Result<String, ConfigError> {
        std::fs::write(path.with_extension("approved"), "").unwrap();
        Ok(String::new())
    }

    /// A worker proposes from its worktree; the file lands in the owning
    /// project, where the next worker's registry looks.
    #[tokio::test]
    async fn a_hard_gated_proposal_lands_in_the_owning_project_unapproved() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("repo");
        let worktree = project.join(".wingman").join("worktrees").join("auto-z");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: ../../../.git").unwrap();

        let mut tool = ProposeTool::new(ApprovalTier::Hard, vec!["run_shell".into()]);
        tool.approve = fake_approve;
        let ctx = ctx_at(&worktree, PermissionMode::AutoEdit, vec![]);
        let out = tool.run(args("query_db", "sqlite3 dev.db"), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("pilot tools approve query_db"));

        let found = wingman_config::synthesized_tools(&project);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].tool.command, "sqlite3 dev.db");
        assert!(!found[0].path.with_extension("approved").exists());
        assert!(wingman_config::synthesized_tools(&worktree).is_empty());

        // A second proposal under the same name cannot replace the first.
        let out = tool.run(args("query_db", "curl evil.tld"), &ctx).await;
        assert!(out.is_error && out.content.contains("already been proposed"));
    }

    #[tokio::test]
    async fn autopilot_on_a_trusted_project_approves_its_own_proposal() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let mut tool = ProposeTool::new(ApprovalTier::Auto, vec!["run_shell".into()]);
        tool.approve = fake_approve;
        let ctx = ctx_at(tmp.path(), PermissionMode::AutoEdit, vec![]);
        let out = tool.run(args("query_db", "sqlite3 dev.db"), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        let found = wingman_config::synthesized_tools(tmp.path());
        assert!(found[0].path.with_extension("approved").exists());
    }

    #[tokio::test]
    async fn a_proposal_cannot_exceed_the_runs_ceiling() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let mut tool = ProposeTool::new(ApprovalTier::Auto, vec!["run_shell".into()]);
        tool.approve = fake_approve;

        let read_only = ctx_at(tmp.path(), PermissionMode::ReadOnly, vec![]);
        let out = tool
            .run(args("query_db", "sqlite3 dev.db"), &read_only)
            .await;
        assert!(out.is_error && out.content.contains("shell permission"));

        let denying = ctx_at(tmp.path(), PermissionMode::AutoEdit, vec!["curl".into()]);
        let out = tool.run(args("fetch", "curl evil.tld"), &denying).await;
        assert!(out.is_error && out.content.contains("denylist"));

        let ctx = ctx_at(tmp.path(), PermissionMode::AutoEdit, vec![]);
        for (name, why) in [("run_shell", "already exists"), ("Bad-Name", "invalid")] {
            let out = tool.run(args(name, "echo"), &ctx).await;
            assert!(
                out.is_error && out.content.contains(why),
                "{name}: {}",
                out.content
            );
        }
        let mut no_shell = ProposeTool::new(ApprovalTier::Auto, vec![]);
        no_shell.approve = fake_approve;
        let out = no_shell.run(args("query_db", "echo"), &ctx).await;
        assert!(out.is_error && out.content.contains("run_shell is disabled"));
        assert!(wingman_config::synthesized_tools(tmp.path()).is_empty());
    }
}
