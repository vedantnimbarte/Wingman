//! `[tools].disabled_tools` and `[tools].preset` hold for tools registered
//! *after* the config was applied.
//!
//! They used to be a one-time sweep, so they only removed what happened to be
//! registered at that instant. Several tools register later than that —
//! `spawn_subagent` and `run_plan` once the `Arc` exists, every MCP tool when
//! its server connects — and naming any of them in `disabled_tools` did
//! nothing at all, with no error to say so.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use wingman_config::PermissionMode;
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_tools::{Tool, ToolCtx, ToolRegistry, ToolRemovals};

struct Named(&'static str);

#[async_trait]
impl Tool for Named {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.0.into(),
            description: "test".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    async fn run(&self, _args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        ToolOutcome::ok("ran")
    }
}

fn registry(removals: ToolRemovals) -> ToolRegistry {
    let tmp = std::env::temp_dir();
    ToolRegistry::new(ToolCtx::new(PermissionMode::ReadOnly, tmp.clone(), tmp))
        .with_tool_removals(removals)
}

/// The reported bug, in its general form.
#[test]
fn a_disabled_tool_cannot_be_registered_later() {
    let reg = registry(ToolRemovals::new(None, vec!["spawn_subagent".into()]));
    // Exactly how runtime.rs registers it: through `&self`, after the config
    // has already been applied.
    let prev = reg.register_arc(Arc::new(Named("spawn_subagent")));

    assert!(prev.is_none(), "should not have displaced anything");
    assert!(
        !reg.tool_names().contains(&"spawn_subagent".to_string()),
        "disabled_tools was ignored for a late registration: {:?}",
        reg.tool_names()
    );
}

/// Anything not named still registers — the guard must not be a blanket
/// refusal of late arrivals.
#[test]
fn an_unlisted_tool_still_registers_later() {
    let reg = registry(ToolRemovals::new(None, vec!["spawn_subagent".into()]));
    reg.register_arc(Arc::new(Named("run_plan")));
    assert!(reg.tool_names().contains(&"run_plan".to_string()));
}

/// MCP tools connect after the session is built, which made them the widest
/// instance of this bug: `disabled_tools` is the obvious knob for turning off
/// one tool from a third-party server, and it did nothing.
#[test]
fn a_preset_keep_list_also_applies_to_later_registrations() {
    let reg = registry(ToolRemovals::new(
        Some(vec!["read_file".into(), "lsp_*".into()]),
        vec![],
    ));
    reg.register_arc(Arc::new(Named("mcp_deploy")));
    reg.register_arc(Arc::new(Named("read_file")));
    reg.register_arc(Arc::new(Named("lsp_hover")));

    let mut names = reg.tool_names();
    names.sort();
    assert_eq!(
        names,
        vec!["lsp_hover".to_string(), "read_file".to_string()],
        "a keep-list must bound later registrations too"
    );
}

/// Denylist beats keep-list: a preset says what a session is *for*, while
/// `disabled_tools` is the standing "not this one, ever".
#[test]
fn the_denylist_wins_over_the_keep_list() {
    let reg = registry(ToolRemovals::new(
        Some(vec!["read_file".into(), "run_shell".into()]),
        vec!["run_shell".into()],
    ));
    reg.register_arc(Arc::new(Named("read_file")));
    reg.register_arc(Arc::new(Named("run_shell")));

    assert_eq!(reg.tool_names(), vec!["read_file".to_string()]);
}

/// No policy configured is the common case and must cost nothing.
#[test]
fn an_empty_policy_excludes_nothing() {
    let reg = registry(ToolRemovals::default());
    reg.register_arc(Arc::new(Named("anything")));
    assert_eq!(reg.tool_names(), vec!["anything".to_string()]);
}

/// `with_builtins` goes through the same guard, so a preset applied before it
/// keeps the registry from ever holding the excluded builtins.
#[test]
fn builtins_registered_after_the_policy_obey_it() {
    let tmp = std::env::temp_dir();
    let reg = ToolRegistry::new(ToolCtx::new(PermissionMode::ReadOnly, tmp.clone(), tmp))
        .with_tool_removals(ToolRemovals::new(None, vec!["run_shell".into()]))
        .with_builtins();

    assert!(
        !reg.tool_names().contains(&"run_shell".to_string()),
        "a disabled builtin was registered anyway: {:?}",
        reg.tool_names()
    );
    // …and the rest are still there.
    assert!(reg.tool_names().contains(&"read_file".to_string()));
}
