//! Model Context Protocol (MCP) client integration.
//!
//! Connects to MCP servers declared in the user's config, lists their tools,
//! and adapts each one as an `wingman_core::ToolDispatcher`-compatible
//! [`McpTool`]. Tools are namespaced as `mcp__<server>__<tool>` so they
//! never collide with built-ins or with another server.
//!
//! Both transports run over rmcp's `RunningService`, so both perform the MCP
//! `initialize` handshake. stdio spawns a child process; http uses rmcp's
//! spec-compliant Streamable-HTTP client (SSE, `Mcp-Session-Id`, auth headers).

use std::collections::HashMap;
use std::sync::Arc;

use wingman_config::McpServerConfig;
use wingman_core::{ToolOutcome, ToolSpec};
use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::{
    model::{CallToolRequestParams, Tool as RmcpTool},
    service::{RoleClient, RunningService},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
        TokioChildProcess,
    },
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
// ServiceMcpClient — wraps an rmcp RunningService (stdio or streamable-http)
// ---------------------------------------------------------------------------

struct ServiceMcpClient {
    inner: Arc<RunningService<RoleClient, ()>>,
    tool_name: String,
}

#[async_trait]
impl McpClient for ServiceMcpClient {
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
// McpServer — holds the connected server + its tool list
// ---------------------------------------------------------------------------

/// One connected MCP server, with its exposed tools.
pub struct McpServer {
    pub name: String,
    /// Underlying client (stdio or http).
    pub client: Arc<dyn McpClient>,
    pub tools: Vec<RmcpTool>,
    /// Whether this server's tools may run in read-only/plan mode. See
    /// [`McpServerConfig::trusted`].
    pub trusted: bool,
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
    // Most MCP servers read their API key / config from the environment, and
    // some must run from a specific directory — pass both through.
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    if let Some(dir) = &cfg.cwd {
        cmd.current_dir(dir);
    }
    let process = TokioChildProcess::new(cmd).map_err(|e| McpError::Transport(e.to_string()))?;
    let service = ().serve(process).await.map_err(|e| McpError::Transport(e.to_string()))?;
    serve_into_server(name, cfg.trusted, service).await
}

async fn connect_http(name: &str, cfg: &McpServerConfig) -> Result<McpServer, McpError> {
    let url = cfg
        .url
        .clone()
        .ok_or_else(|| McpError::Config("http transport requires `url`".into()))?;

    // rmcp's Streamable-HTTP client does the full MCP handshake: `initialize`
    // + `notifications/initialized`, SSE responses, `Mcp-Session-Id` tracking,
    // and re-init on session expiry. We only supply the URL + auth/custom
    // headers from config.
    let mut headers: HashMap<HeaderName, HeaderValue> = HashMap::new();
    for (k, v) in &cfg.headers {
        match (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
            (Ok(hn), Ok(hv)) => {
                headers.insert(hn, hv);
            }
            _ => warn!("mcp {name}: skipping invalid header {k:?}"),
        }
    }
    let config = StreamableHttpClientTransportConfig::with_uri(url).custom_headers(headers);
    let transport = StreamableHttpClientTransport::from_config(config);
    let service = ()
        .serve(transport)
        .await
        .map_err(|e| McpError::Transport(e.to_string()))?;
    serve_into_server(name, cfg.trusted, service).await
}

/// Shared tail: list the server's tools and wrap the running service as an
/// [`McpServer`]. Works for both transports since both yield a
/// `RunningService<RoleClient, ()>`.
async fn serve_into_server(
    name: &str,
    trusted: bool,
    service: RunningService<RoleClient, ()>,
) -> Result<McpServer, McpError> {
    let service = Arc::new(service);
    let tools = service
        .list_all_tools()
        .await
        .map_err(|e| McpError::Rpc(e.to_string()))?;
    let client: Arc<dyn McpClient> = Arc::new(ServiceMcpClient {
        inner: service,
        tool_name: name.to_string(),
    });
    Ok(McpServer {
        name: name.to_string(),
        client,
        tools,
        trusted,
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
/// `Tool` trait shape that `wingman-tools` expects. We define it here
/// (and not in `wingman-tools`) so that crate stays MCP-free; callers
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

/// Implements the same shape as `wingman_tools::Tool` without taking that
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A client that records the last call and returns a canned reply, so the
    /// McpTool adapter can be exercised without spawning a real server.
    struct FakeClient {
        /// `Ok(text)` on success, `Err(msg)` to surface an rpc error.
        reply: Result<String, String>,
    }

    #[async_trait]
    impl McpClient for FakeClient {
        async fn call_tool(
            &self,
            _name: &str,
            _args: Option<serde_json::Map<String, serde_json::Value>>,
        ) -> Result<String, McpError> {
            self.reply.clone().map_err(McpError::Rpc)
        }
    }

    fn tool(server: &str, name: &str, reply: Result<String, String>) -> McpTool {
        let schema = Arc::new(serde_json::Map::new());
        let server = McpServer {
            name: server.into(),
            client: Arc::new(FakeClient { reply }),
            tools: vec![],
            trusted: false,
        };
        McpTool::build(&server, &RmcpTool::new(name.to_string(), "desc", schema))
    }

    /// Tools are namespaced `mcp__<server>__<tool>` so they can't collide with
    /// built-ins or another server's tools.
    #[test]
    fn full_name_is_namespaced() {
        let t = tool("fs", "read_file", Ok(String::new()));
        assert_eq!(t.full_name(), "mcp__fs__read_file");
        assert_eq!(t.spec().name, "mcp__fs__read_file");
    }

    /// A successful call surfaces the server's text as a non-error outcome.
    #[tokio::test]
    async fn run_maps_ok_to_outcome() {
        let t = tool("fs", "read_file", Ok("contents".into()));
        let out = t.run(serde_json::json!({})).await;
        assert!(!out.is_error);
        assert_eq!(out.content, "contents");
    }

    /// A transport/rpc error becomes an error outcome, not a panic.
    #[tokio::test]
    async fn run_maps_err_to_error_outcome() {
        let t = tool("fs", "read_file", Err("boom".into()));
        let out = t.run(serde_json::json!({})).await;
        assert!(out.is_error);
    }

    /// Non-object args are rejected before ever reaching the server.
    #[tokio::test]
    async fn run_rejects_non_object_args() {
        let t = tool("fs", "read_file", Ok(String::new()));
        let out = t.run(serde_json::json!("just a string")).await;
        assert!(out.is_error);
        assert!(out.content.contains("expected object args"));
    }

    /// Unknown transports are a config error, not a connect attempt.
    #[tokio::test]
    async fn connect_rejects_unknown_transport() {
        let cfg = McpServerConfig {
            transport: "carrier-pigeon".into(),
            ..Default::default()
        };
        match connect("x", &cfg).await {
            Err(McpError::Config(_)) => {}
            other => panic!("expected Config error, got {:?}", other.err()),
        }
    }
}
