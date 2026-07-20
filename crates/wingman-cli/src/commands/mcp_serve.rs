//! `wingman mcp-serve` — expose Wingman itself as an MCP **server**.
//!
//! Wingman is an MCP *host* (it consumes external MCP servers). This flips it:
//! any MCP client (Claude Code, Cursor, another Wingman) can consume Wingman's
//! built-in tools over stdio — most valuably `semantic_search` (the warm repo
//! index) and `recall_memory` (git-backed team memory), plus the `lsp_*`
//! intelligence. That turns Wingman from "another agent" into infrastructure
//! other agents plug into.
//!
//! Transport: MCP stdio — newline-delimited JSON-RPC 2.0 (one message per line,
//! no Content-Length framing). We implement the subset clients need:
//! `initialize`, `tools/list`, `tools/call`, `resources/list`, `resources/read`,
//! `ping`, and the `notifications/initialized` no-op.
//!
//! Safety: defaults to read-only permission, so a connected client can search,
//! read, and recall but not write/execute unless the operator explicitly raises
//! the mode with `--mode`.

use anyhow::Result;
use serde_json::{json, Value};
use std::process::ExitCode;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use wingman_config::{Config, PermissionMode, ProjectPaths};
use wingman_core::ToolDispatcher;

/// MCP protocol version we implement; we echo the client's request when we can.
const DEFAULT_PROTOCOL: &str = "2024-11-05";

pub async fn run(cfg: Config, mode: PermissionMode) -> Result<ExitCode> {
    let registry = crate::runtime::build_registry(&cfg, mode).await?;
    let paths = ProjectPaths::discover(&std::env::current_dir()?);

    eprintln!(
        "wingman mcp-serve: exposing {} tools over stdio (mode: {:?}). Connect an MCP client to this process.",
        registry.tool_names().len(),
        mode
    );

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break; // client closed the pipe
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue, // skip a malformed line, keep serving
        };

        // Notifications have no `id`; requests do. We only reply to requests.
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        let response = match handle(method, &params, &registry, &paths).await {
            HandleResult::Reply(result) => id.map(|id| json!({
                "jsonrpc": "2.0", "id": id, "result": result
            })),
            HandleResult::Error(code, message) => id.map(|id| json!({
                "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message }
            })),
            HandleResult::NoReply => None,
        };

        if let Some(resp) = response {
            let mut body = serde_json::to_string(&resp)?;
            body.push('\n');
            stdout.write_all(body.as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

enum HandleResult {
    Reply(Value),
    Error(i64, String),
    NoReply,
}

async fn handle(
    method: &str,
    params: &Value,
    registry: &wingman_tools::ToolRegistry,
    paths: &ProjectPaths,
) -> HandleResult {
    match method {
        "initialize" => {
            let protocol = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL)
                .to_string();
            HandleResult::Reply(json!({
                "protocolVersion": protocol,
                "capabilities": {
                    "tools": { "listChanged": false },
                    "resources": { "listChanged": false, "subscribe": false }
                },
                "serverInfo": { "name": "wingman", "version": env!("CARGO_PKG_VERSION") }
            }))
        }
        // Client acknowledgements — no reply.
        "notifications/initialized" | "initialized" => HandleResult::NoReply,
        "ping" => HandleResult::Reply(json!({})),
        "tools/list" => {
            let tools: Vec<Value> = registry
                .specs()
                .into_iter()
                .map(|s| json!({
                    "name": s.name,
                    "description": s.description,
                    "inputSchema": s.input_schema,
                }))
                .collect();
            HandleResult::Reply(json!({ "tools": tools }))
        }
        "tools/call" => {
            let name = match params.get("name").and_then(Value::as_str) {
                Some(n) => n.to_string(),
                None => return HandleResult::Error(-32602, "missing tool name".into()),
            };
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            let outcome = registry.dispatch(&name, arguments).await;
            HandleResult::Reply(json!({
                "content": [ { "type": "text", "text": outcome.content } ],
                "isError": outcome.is_error
            }))
        }
        "resources/list" => {
            let resources = memory_resources(paths);
            HandleResult::Reply(json!({ "resources": resources }))
        }
        "resources/read" => {
            let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
            match read_memory_resource(paths, uri) {
                Some(text) => HandleResult::Reply(json!({
                    "contents": [ { "uri": uri, "mimeType": "text/markdown", "text": text } ]
                })),
                None => HandleResult::Error(-32602, format!("unknown resource: {uri}")),
            }
        }
        // Unknown method: a JSON-RPC method-not-found error (only if a request).
        _ => HandleResult::Error(-32601, format!("method not found: {method}")),
    }
}

/// Expose each project + global memory as an MCP resource so a connected agent
/// can pull the team's accumulated knowledge, not just call tools.
fn memory_resources(paths: &ProjectPaths) -> Vec<Value> {
    let store = wingman_learn::memory::MemoryStore::new(paths.root.clone());
    store
        .load_all()
        .into_iter()
        .map(|m| json!({
            "uri": format!("wingman-memory:///{}", m.name),
            "name": m.name,
            "description": m.description,
            "mimeType": "text/markdown",
        }))
        .collect()
}

fn read_memory_resource(paths: &ProjectPaths, uri: &str) -> Option<String> {
    let slug = uri.strip_prefix("wingman-memory:///")?;
    let store = wingman_learn::memory::MemoryStore::new(paths.root.clone());
    store.find(slug).map(|m| m.body)
}
