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
}

impl McpToolAdapter {
    pub fn new(handle: Arc<dyn McpToolHandle>) -> Self {
        Self { inner: handle }
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        self.inner.run(args).await
    }
}
