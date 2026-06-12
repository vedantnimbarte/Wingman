//! `arccode mcp-serve` — expose Arc-Code's tool registry as an MCP server
//! over stdio, so MCP hosts (Claude Desktop, editors, other agents) can call
//! Arc-Code's tools — including semantic_search backed by the project index.
//!
//! The MCP stdio transport is newline-delimited JSON-RPC 2.0; the surface we
//! implement is initialize / tools/list / tools/call / ping, which is all a
//! tools-only server needs. Hand-rolled on purpose: no extra dependency, and
//! the message shapes are stable.

use anyhow::Result;
use arccode_config::{Config, PermissionMode, ProjectPaths};
use arccode_core::ToolDispatcher;
use arccode_tools::{ToolCtx, ToolRegistry};
use serde_json::{json, Value};
use std::process::ExitCode;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL_VERSION: &str = "2024-11-05";

pub async fn run(cfg: Config, mode_flag: Option<PermissionMode>) -> Result<ExitCode> {
    // Default to read-only: an external host calling our tools should not
    // get write/shell access unless the operator explicitly grants it.
    // The project policy ceiling still applies on top.
    let mode = cfg.clamp_mode(mode_flag.unwrap_or(PermissionMode::ReadOnly));

    let cwd = std::env::current_dir().unwrap_or_default();
    let paths = ProjectPaths::discover(&cwd);
    let ctx = ToolCtx::new_with_config(
        mode,
        cwd,
        paths.root.clone(),
        cfg.tools.shell_denylist.clone(),
    );
    let mut registry = ToolRegistry::new(ctx).with_builtins();
    if let Ok(Some(idx)) = crate::runtime::build_indexer(&paths) {
        registry = registry.with_semantic_search(idx);
    }
    let registry = Arc::new(registry);

    eprintln!(
        "arccode mcp server: {} tools, mode {mode}, project {}",
        registry.specs().len(),
        paths.root.display()
    );

    let stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut lines = stdin.lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_msg(
                    &mut stdout,
                    &rpc_error(Value::Null, -32700, &format!("parse error: {e}")),
                )
                .await?;
                continue;
            }
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");

        // Notifications (no id) get no response.
        let Some(id) = id else {
            continue;
        };

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": msg["params"]["protocolVersion"]
                        .as_str()
                        .unwrap_or(PROTOCOL_VERSION),
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "arccode",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }
            }),
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => {
                let tools: Vec<Value> = registry
                    .specs()
                    .into_iter()
                    .map(|s| {
                        json!({
                            "name": s.name,
                            "description": s.description,
                            "inputSchema": s.input_schema,
                        })
                    })
                    .collect();
                json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } })
            }
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or("");
                let args = msg["params"]["arguments"].clone();
                let args = if args.is_null() { json!({}) } else { args };
                let outcome = registry.dispatch(name, args).await;
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": outcome.content }],
                        "isError": outcome.is_error,
                    }
                })
            }
            other => rpc_error(id, -32601, &format!("method not found: {other}")),
        };
        write_msg(&mut stdout, &response).await?;
    }

    Ok(ExitCode::SUCCESS)
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

async fn write_msg(stdout: &mut tokio::io::Stdout, msg: &Value) -> Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    stdout.write_all(line.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}
