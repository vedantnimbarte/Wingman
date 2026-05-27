//! Bridges `arccode_mcp::McpTool` into the `arccode_tools::Tool` trait so
//! MCP-served tools live in the same `ToolRegistry` as built-ins.
//!
//! Kept inside `arccode-cli` (and not in `arccode-tools`) so the tools
//! crate stays MCP-free and the dependency graph remains one-way.

use std::sync::Arc;

use arccode_core::{ToolOutcome, ToolSpec};
use arccode_mcp::{McpServer, McpTool, McpToolHandle};
use arccode_tools::{Tool, ToolCtx};
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

/// Build adapters from a list of connected servers.
pub fn build_adapters(servers: &[McpServer]) -> Vec<McpToolAdapter> {
    let mut out = Vec::new();
    for server in servers {
        for tool in &server.tools {
            let handle: Arc<dyn McpToolHandle> = Arc::new(McpTool::build(server, tool));
            out.push(McpToolAdapter::new(handle));
        }
    }
    out
}
