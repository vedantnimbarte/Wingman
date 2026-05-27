//! Model Context Protocol (MCP) client integration.
//!
//! Connects to MCP servers declared in the user's config, lists their tools,
//! and adapts each one as an `arccode_core::ToolDispatcher`-compatible
//! [`McpTool`]. Tools are namespaced as `mcp__<server>__<tool>` so they
//! never collide with built-ins or with another server.
//!
//! M3 ships stdio transport (most servers ship as a process). HTTP is a
//! straightforward follow-up via `rmcp`'s `transport-streamable-http-client`.

use std::sync::Arc;

use arccode_config::McpServerConfig;
use arccode_core::{ToolOutcome, ToolSpec};
use async_trait::async_trait;
use rmcp::{
    model::{CallToolRequestParams, Tool as RmcpTool},
    service::{RoleClient, RunningService},
    transport::TokioChildProcess,
    ServiceExt,
};
use thiserror::Error;
use tokio::process::Command;
use tracing::warn;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("rpc: {0}")]
    Rpc(String),
    #[error("bad config: {0}")]
    Config(String),
}

/// One connected MCP server, with its exposed tools.
pub struct McpServer {
    pub name: String,
    pub client: Arc<RunningService<RoleClient, ()>>,
    pub tools: Vec<RmcpTool>,
}

/// Connect to a single server based on user config.
pub async fn connect(name: &str, cfg: &McpServerConfig) -> Result<McpServer, McpError> {
    match cfg.transport.as_str() {
        "stdio" => connect_stdio(name, cfg).await,
        "http" => Err(McpError::Config(
            "http transport lands after stdio in M3".into(),
        )),
        other => Err(McpError::Config(format!("unknown transport: {other}"))),
    }
}

async fn connect_stdio(name: &str, cfg: &McpServerConfig) -> Result<McpServer, McpError> {
    let command = cfg
        .command
        .as_deref()
        .ok_or_else(|| McpError::Config("stdio transport requires `command`".into()))?;
    let mut cmd = Command::new(command);
    for a in &cfg.args {
        cmd.arg(a);
    }
    let process = TokioChildProcess::new(cmd).map_err(|e| McpError::Transport(e.to_string()))?;
    let client = ().serve(process).await.map_err(|e| McpError::Transport(e.to_string()))?;

    let tools = client
        .list_all_tools()
        .await
        .map_err(|e| McpError::Rpc(e.to_string()))?;

    Ok(McpServer {
        name: name.to_string(),
        client: Arc::new(client),
        tools,
    })
}

/// Connect every declared server. Servers that fail to start are logged
/// and skipped — one broken server should not take the whole session down.
pub async fn connect_all(
    servers: &std::collections::BTreeMap<String, McpServerConfig>,
) -> Vec<McpServer> {
    let mut out = Vec::new();
    for (name, cfg) in servers {
        match connect(name, cfg).await {
            Ok(s) => {
                tracing::info!("mcp: connected to {} ({} tools)", name, s.tools.len());
                out.push(s);
            }
            Err(e) => {
                warn!("mcp: failed to connect to {name}: {e}");
            }
        }
    }
    out
}

/// Adapter type: one entry per exposed MCP tool, implementing the same
/// `Tool` trait shape that `arccode-tools` expects. We define it here
/// (and not in `arccode-tools`) so that crate stays MCP-free; callers
/// register these with `ToolRegistry::register`.
pub struct McpTool {
    server: Arc<RunningService<RoleClient, ()>>,
    server_name: String,
    tool_name: String,
    description: String,
    input_schema: serde_json::Value,
}

impl McpTool {
    pub fn build(server: &McpServer, tool: &RmcpTool) -> Self {
        Self {
            server: server.client.clone(),
            server_name: server.name.clone(),
            tool_name: tool.name.to_string(),
            description: tool.description.as_deref().unwrap_or("").to_string(),
            input_schema: serde_json::to_value(&tool.input_schema)
                .unwrap_or_else(|_| serde_json::json!({ "type": "object" })),
        }
    }

    /// Spec name as exposed to the model.
    pub fn full_name(&self) -> String {
        format!("mcp__{}__{}", self.server_name, self.tool_name)
    }
}

/// Implements the same shape as `arccode_tools::Tool` without taking that
/// crate as a dependency. The runtime adapts on registration.
#[async_trait]
pub trait McpToolHandle: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn run(&self, args: serde_json::Value) -> ToolOutcome;
}

#[async_trait]
impl McpToolHandle for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.full_name(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let arguments = match args {
            serde_json::Value::Object(m) => Some(m),
            serde_json::Value::Null => None,
            other => {
                return ToolOutcome::err(format!(
                    "mcp tool {} expected object args, got {}",
                    self.tool_name, other
                ))
            }
        };
        let mut req = CallToolRequestParams::new(self.tool_name.clone());
        if let Some(args) = arguments {
            req = req.with_arguments(args);
        }
        match self.server.call_tool(req).await {
            Ok(result) => {
                let mut text = String::new();
                for content in result.content {
                    if let Some(t) = content.as_text() {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&t.text);
                    }
                }
                if result.is_error.unwrap_or(false) {
                    ToolOutcome::err(text)
                } else {
                    ToolOutcome::ok(text)
                }
            }
            Err(e) => ToolOutcome::err(format!("mcp {}: {e}", self.tool_name)),
        }
    }
}
