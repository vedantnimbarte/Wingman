//! Bridges `wingman_mcp::McpTool` into the `wingman_tools::Tool` trait so
//! MCP-served tools live in the same `ToolRegistry` as built-ins.
//!
//! Kept inside `wingman-cli` (and not in `wingman-tools`) so the tools
//! crate stays MCP-free and the dependency graph remains one-way.

use std::sync::Arc;

use wingman_core::{ToolOutcome, ToolSpec};
use wingman_mcp::McpToolHandle;
use wingman_tools::{Tool, ToolCtx};
use async_trait::async_trait;
use serde_json::Value;

pub struct McpToolAdapter {
    inner: Arc<dyn McpToolHandle>,
    /// Whether the owning server is trusted to run in read-only/plan mode.
    trusted: bool,
}

impl McpToolAdapter {
    pub fn new(handle: Arc<dyn McpToolHandle>, trusted: bool) -> Self {
        Self {
            inner: handle,
            trusted,
        }
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        // MCP tools are opaque — we can't tell a read-only search tool from one
        // that writes files or runs commands. Unless the server is explicitly
        // trusted, gate them to edit-capable modes (auto-edit/yolo), the same
        // bar as the shell tool, so they can't act in read-only/plan mode.
        if !self.trusted && !ctx.allows_shell() {
            return ToolOutcome::err(format!(
                "mcp tool denied in {:?} mode: this server is not marked `trusted`; \
                 switch to auto-edit/yolo or set `trusted = true` for it in config",
                ctx.mode()
            ));
        }
        self.inner.run(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wingman_config::PermissionMode;

    struct FakeHandle;

    #[async_trait]
    impl McpToolHandle for FakeHandle {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "mcp__x__y".into(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }
        async fn run(&self, _args: Value) -> ToolOutcome {
            ToolOutcome::ok("ran")
        }
    }

    fn adapter(trusted: bool) -> McpToolAdapter {
        McpToolAdapter::new(Arc::new(FakeHandle), trusted)
    }

    #[tokio::test]
    async fn untrusted_denied_in_read_only_allowed_in_auto_edit() {
        let root = std::env::temp_dir();
        let ctx = ToolCtx::new(PermissionMode::ReadOnly, root.clone(), root.clone());
        let denied = adapter(false).run(serde_json::json!({}), &ctx).await;
        assert!(denied.is_error);
        assert!(denied.content.contains("not marked `trusted`"));

        ctx.set_mode(PermissionMode::AutoEdit);
        let ok = adapter(false).run(serde_json::json!({}), &ctx).await;
        assert!(!ok.is_error);
        assert_eq!(ok.content, "ran");
    }

    #[tokio::test]
    async fn trusted_runs_in_read_only() {
        let root = std::env::temp_dir();
        let ctx = ToolCtx::new(PermissionMode::ReadOnly, root.clone(), root.clone());
        let ok = adapter(true).run(serde_json::json!({}), &ctx).await;
        assert!(!ok.is_error);
    }
}
