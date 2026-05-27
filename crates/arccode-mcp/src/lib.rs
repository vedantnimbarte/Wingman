//! Model Context Protocol (MCP) client integration.
//!
//! Connects to MCP servers declared in the user's config, lists their tools,
//! and adapts each one as an `arccode_core::ToolDispatcher`-compatible
//! [`McpTool`]. Tools are namespaced as `mcp__<server>__<tool>` so they
//! never collide with built-ins or with another server.
//!
//! M3 ships stdio transport (most servers ship as a process). HTTP transport
//! uses JSON-RPC 2.0 style requests over `reqwest`.

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
    #[error("http: {0}")]
    Http(String),
}

// ---------------------------------------------------------------------------
// McpClient trait — abstraction over stdio (RunningService) and HTTP.
// ---------------------------------------------------------------------------

/// Abstraction over different MCP transport backends.
#[async_trait]
pub trait McpClient: Send + Sync {
    async fn call_tool(
        &self,
        name: &str,
        args: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<String, McpError>;
}

// ---------------------------------------------------------------------------
// Stdio McpClient — wraps rmcp RunningService
// ---------------------------------------------------------------------------

struct StdioMcpClient {
    inner: Arc<RunningService<RoleClient, ()>>,
    tool_name: String,
}

#[async_trait]
impl McpClient for StdioMcpClient {
    async fn call_tool(
        &self,
        name: &str,
        args: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<String, McpError> {
        let mut req = CallToolRequestParams::new(name.to_string());
        if let Some(a) = args {
            req = req.with_arguments(a);
        }
        match self.inner.call_tool(req).await {
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
                    Err(McpError::Rpc(text))
                } else {
                    Ok(text)
                }
            }
            Err(e) => Err(McpError::Rpc(format!("mcp {}: {e}", self.tool_name))),
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP McpClient — JSON-RPC 2.0 over HTTP via reqwest
// ---------------------------------------------------------------------------

struct HttpMcpClient {
    base_url: String,
    http: reqwest::Client,
}

impl HttpMcpClient {
    fn new(base_url: String) -> Self {
        Self {
            base_url,
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl McpClient for HttpMcpClient {
    async fn call_tool(
        &self,
        name: &str,
        args: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<String, McpError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": args.unwrap_or_default()
            },
            "id": 1
        });

        let resp = self
            .http
            .post(&self.base_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| McpError::Http(e.to_string()))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| McpError::Http(e.to_string()))?;

        if !status.is_success() {
            return Err(McpError::Http(format!("HTTP {status}: {text}")));
        }

        // Parse JSON-RPC 2.0 response.
        let val: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| McpError::Http(e.to_string()))?;

        if let Some(err) = val.get("error") {
            return Err(McpError::Rpc(err.to_string()));
        }

        let result = val.get("result").unwrap_or(&serde_json::Value::Null);

        // Try to extract text from content array (standard MCP shape).
        if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
            let mut out = String::new();
            for item in content {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(t);
                    }
                }
            }
            return Ok(out);
        }

        // Fallback: stringify whatever the result is.
        Ok(result.to_string())
    }
}

// ---------------------------------------------------------------------------
// McpServer — holds the connected server + its tool list
// ---------------------------------------------------------------------------

/// One connected MCP server, with its exposed tools.
pub struct McpServer {
    pub name: String,
    /// Underlying client (stdio or http).
    pub client: Arc<dyn McpClient>,
    pub tools: Vec<RmcpTool>,
}

/// Connect to a single server based on user config.
pub async fn connect(name: &str, cfg: &McpServerConfig) -> Result<McpServer, McpError> {
    match cfg.transport.as_str() {
        "stdio" => connect_stdio(name, cfg).await,
        "http" => connect_http(name, cfg).await,
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
    let service = ().serve(process).await.map_err(|e| McpError::Transport(e.to_string()))?;
    let service = Arc::new(service);

    let tools = service
        .list_all_tools()
        .await
        .map_err(|e| McpError::Rpc(e.to_string()))?;

    // Build per-server stdio client (shared Arc for all tools).
    let client: Arc<dyn McpClient> = Arc::new(StdioMcpClient {
        inner: service,
        tool_name: name.to_string(),
    });

    Ok(McpServer {
        name: name.to_string(),
        client,
        tools,
    })
}

async fn connect_http(name: &str, cfg: &McpServerConfig) -> Result<McpServer, McpError> {
    let base_url = cfg
        .url
        .clone()
        .ok_or_else(|| McpError::Config("http transport requires `url`".into()))?;

    let http_client = reqwest::Client::new();

    // Fetch the tool list via JSON-RPC 2.0 `tools/list`.
    let list_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "params": {},
        "id": 1
    });

    let resp = http_client
        .post(&base_url)
        .json(&list_body)
        .send()
        .await
        .map_err(|e| McpError::Http(e.to_string()))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| McpError::Http(e.to_string()))?;

    if !status.is_success() {
        return Err(McpError::Http(format!(
            "HTTP {status} listing tools for {name}: {text}"
        )));
    }

    let val: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| McpError::Http(e.to_string()))?;

    if let Some(err) = val.get("error") {
        return Err(McpError::Rpc(format!(
            "tools/list error for {name}: {err}"
        )));
    }

    let result = val.get("result").unwrap_or(&serde_json::Value::Null);
    let tools_json = result
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();

    let tools: Vec<RmcpTool> = tools_json
        .into_iter()
        .filter_map(|t| serde_json::from_value(t).ok())
        .collect();

    let client: Arc<dyn McpClient> = Arc::new(HttpMcpClient::new(base_url));

    Ok(McpServer {
        name: name.to_string(),
        client,
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
    client: Arc<dyn McpClient>,
    server_name: String,
    tool_name: String,
    description: String,
    input_schema: serde_json::Value,
}

impl McpTool {
    pub fn build(server: &McpServer, tool: &RmcpTool) -> Self {
        Self {
            client: server.client.clone(),
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
        match self.client.call_tool(&self.tool_name, arguments).await {
            Ok(text) => ToolOutcome::ok(text),
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    }
}
