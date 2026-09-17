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
    let paths = ProjectPaths::discover(&std::env::current_dir()?);
    // Build the learn handles up front: without them `build_registry` drops
    // `recall_memory`, `save_memory`, `forget_memory`, `invoke_skill`,
    // `recall_session` and `read_session`, so a connected client saw a
    // registry missing exactly the half this command's docs advertise.
    // Writes stay gated by `mode` (read-only by default), not by absence.
    let learn =
        crate::runtime::build_learn(&cfg, &paths, format!("mcp-serve-{}", std::process::id()));
    let registry = crate::runtime::build_registry_with_learn(&cfg, mode, learn).await?;

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

        let response = match handle(method, &params, &registry, &paths, mode).await {
            HandleResult::Reply(result) => id.map(|id| {
                json!({
                    "jsonrpc": "2.0", "id": id, "result": result
                })
            }),
            HandleResult::Error(code, message) => id.map(|id| {
                json!({
                    "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message }
                })
            }),
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
    mode: PermissionMode,
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
            let mut tools: Vec<Value> = registry
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
            tools.extend(pilot_tools(mode));
            HandleResult::Reply(json!({ "tools": tools }))
        }
        "tools/call" => {
            let name = match params.get("name").and_then(Value::as_str) {
                Some(n) => n.to_string(),
                None => return HandleResult::Error(-32602, "missing tool name".into()),
            };
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            if let Some(reply) = call_pilot_tool(&name, &arguments, paths, mode).await {
                return HandleResult::Reply(reply);
            }
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
        .map(|m| {
            json!({
                "uri": format!("wingman-memory:///{}", m.name),
                "name": m.name,
                "description": m.description,
                "mimeType": "text/markdown",
            })
        })
        .collect()
}

fn read_memory_resource(paths: &ProjectPaths, uri: &str) -> Option<String> {
    let slug = uri.strip_prefix("wingman-memory:///")?;
    let store = wingman_learn::memory::MemoryStore::new(paths.root.clone());
    store.find(slug).map(|m| m.body)
}

/// Pilot as tools, so another agent (Claude Code, Cursor) can hand Wingman a
/// whole goal and follow it. `pilot_status` is a read; `pilot_run` starts
/// workers that write, so it is only offered when `--mode` allows writes, and
/// the run inherits that mode as its ceiling (as `wingman serve` does).
fn pilot_tools(mode: PermissionMode) -> Vec<Value> {
    let mut tools = vec![json!({
        "name": "pilot_status",
        "description": "List Wingman pilot runs in this project, or with `run_id` return one run's \
            full state: status, tasks with outcomes, agents, spend and PR URL.",
        "inputSchema": {"type": "object", "properties": {
            "run_id": {"type": "string", "description": "Run to inspect; omit to list runs."}
        }},
    })];
    if writes_allowed(mode) {
        tools.push(json!({
            "name": "pilot_run",
            "description": "Start a Wingman pilot run in the background: plan the goal into tasks, \
                run worker agents in isolated git worktrees, merge and open a PR. Returns the run \
                id at once; follow it with `pilot_status`. Without `yes` the run waits at the plan \
                gate for `wingman pilot approve`.",
            "inputSchema": {"type": "object", "required": ["goal"], "properties": {
                "goal": {"type": "string"},
                "yes": {"type": "boolean", "description": "Approve the plan automatically."},
                "plan_only": {"type": "boolean"},
                "model": {"type": "string", "description": "provider/model, e.g. claude-code/sonnet"},
                "max_usd": {"type": "number"}
            }},
        }));
    }
    tools
}

fn writes_allowed(mode: PermissionMode) -> bool {
    matches!(mode, PermissionMode::AutoEdit | PermissionMode::Yolo)
}

/// Handle a pilot tool call, or `None` when `name` is not one.
async fn call_pilot_tool(
    name: &str,
    args: &Value,
    paths: &ProjectPaths,
    mode: PermissionMode,
) -> Option<Value> {
    let text = |t: String, is_error: bool| json!({ "content": [ { "type": "text", "text": t } ], "isError": is_error });
    Some(match name {
        "pilot_status" => match args.get("run_id").and_then(Value::as_str) {
            Some(id) => match crate::serve::pilot::run_state(&paths.root, id) {
                Some(state) => text(
                    serde_json::to_string_pretty(&state).unwrap_or_default(),
                    false,
                ),
                None => text(format!("no pilot run `{id}` in this project"), true),
            },
            None => match crate::serve::pilot::runs_json(&paths.root) {
                Ok(v) => text(v.to_string(), false),
                Err(e) => text(e, true),
            },
        },
        "pilot_run" if !writes_allowed(mode) => text(
            "pilot_run needs `wingman mcp-serve --mode auto-edit` (workers write code)".into(),
            true,
        ),
        "pilot_run" => {
            let body: crate::serve::pilot::StartBody = match serde_json::from_value(args.clone()) {
                Ok(b) => b,
                Err(e) => return Some(text(format!("bad arguments: {e}"), true)),
            };
            if body.goal.trim().is_empty() {
                return Some(text("a run needs a non-empty `goal`".into(), true));
            }
            match crate::serve::pilot::spawn_detached_run(&paths.root, &body, mode).await {
                Ok(run_id) => text(
                    format!("started pilot run {run_id}; follow it with pilot_status"),
                    false,
                ),
                Err(e) => text(e.to_string(), true),
            }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pilot_run_is_only_offered_when_writes_are() {
        let names = |m| {
            pilot_tools(m)
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(PermissionMode::ReadOnly), ["pilot_status"]);
        assert_eq!(
            names(PermissionMode::AutoEdit),
            ["pilot_status", "pilot_run"]
        );
    }

    #[tokio::test]
    async fn read_only_refuses_pilot_run_and_status_reads_runs() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::discover(dir.path());
        let refused = call_pilot_tool(
            "pilot_run",
            &json!({"goal": "x"}),
            &paths,
            PermissionMode::ReadOnly,
        )
        .await
        .unwrap();
        assert_eq!(refused["isError"], true);
        let missing = call_pilot_tool(
            "pilot_status",
            &json!({"run_id": "../etc"}),
            &paths,
            PermissionMode::ReadOnly,
        )
        .await
        .unwrap();
        assert_eq!(missing["isError"], true);
        assert!(
            call_pilot_tool("read_file", &json!({}), &paths, PermissionMode::ReadOnly)
                .await
                .is_none()
        );
    }
}
