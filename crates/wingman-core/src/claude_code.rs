//! Run the user's own Claude Code CLI as an agent.
//!
//! This is how a Claude subscription (Pro / Max) drives Wingman without an API
//! key. Wingman never touches Claude credentials: it spawns the unmodified
//! `claude` binary the user installed and signed in to themselves, and reads
//! what it prints. Anthropic's terms allow exactly that and forbid the
//! alternative (Wingman offering Claude login or reusing its OAuth tokens), so
//! do not "optimise" this into calling the API with the CLI's token.
//!
//! Claude Code runs its own agent loop and its own tools, so it cannot sit
//! behind [`crate::Provider`] the way an HTTP model does. Instead [`ClaudeCode`]
//! is a peer of [`crate::AgentLoop`]: `run(prompt)` yields the same
//! [`AgentEvent`] stream, translated from `--output-format stream-json`.
//!
//! Wingman's own tools (a worker's `task_complete`, the manager's
//! `assign_task`, …) reach the model through [`McpBridge`]: a loopback MCP
//! endpoint, guarded by a per-run bearer token, that dispatches into the same
//! [`ToolDispatcher`] the native loop would have used. Permissions, audit and
//! removals therefore apply unchanged.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use futures::stream::BoxStream;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::{AgentEvent, AgentStop, ToolDispatcher, ToolSpec, Usage};

/// The provider id that selects Claude Code: `--model claude-code/sonnet`.
pub const PROVIDER_ID: &str = "claude-code";

/// MCP server name the bridge is registered under. Claude Code prefixes its
/// tools with `mcp__wingman__`; events strip that back off.
const BRIDGE_NAME: &str = "wingman";
const BRIDGE_PREFIX: &str = "mcp__wingman__";

/// Wingman tools to expose over MCP, optionally narrowed to these names.
pub type Bridge = (Arc<dyn ToolDispatcher>, Option<Vec<String>>);

/// One Claude Code agent. Configure the public fields, then call [`run`].
///
/// [`run`]: ClaudeCode::run
pub struct ClaudeCode {
    /// The CLI to spawn: `$WINGMAN_CLAUDE_BIN`, else `claude` on PATH.
    pub bin: String,
    /// `--model`: an alias (`sonnet`, `opus`) or a full model id. Empty lets
    /// Claude Code pick.
    pub model: String,
    pub cwd: PathBuf,
    /// Appended to Claude Code's own system prompt (role prompts).
    pub append_system: Option<String>,
    /// Replaces Claude Code's system prompt (plain completions).
    pub system: Option<String>,
    /// `--tools`: `None` keeps the built-in set, `Some("")` disables it.
    pub tools: Option<String>,
    /// `--allowedTools`. Anything else that would prompt is denied: there is
    /// nobody to ask.
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    /// `--permission-mode` (`default`, `acceptEdits`, `plan`, …).
    pub permission_mode: String,
    pub max_turns: Option<usize>,
    /// Wingman tools to expose over MCP (see [`Bridge`]).
    pub bridge: Option<Bridge>,
    /// Continue the same Claude Code session on the next `run`, the way an
    /// `AgentLoop` keeps its history.
    pub resume: bool,
    /// Don't write a Claude Code session file (one-shot side calls).
    pub ephemeral: bool,
    /// Messages to send into a running turn (pilot pivot / clarify). Drained
    /// whenever Claude Code prints something.
    pub inbox: Option<Arc<Mutex<Vec<String>>>>,
    /// Called when the subscription's rate limit rejects a request, with the
    /// seconds until it resets when known.
    pub on_rate_limit: Option<Arc<dyn Fn(Option<u64>) + Send + Sync>>,
    session_id: Option<String>,
}

impl ClaudeCode {
    pub fn new(model: impl Into<String>, cwd: PathBuf) -> Self {
        Self {
            bin: std::env::var("WINGMAN_CLAUDE_BIN").unwrap_or_else(|_| "claude".into()),
            model: model.into(),
            cwd,
            append_system: None,
            system: None,
            tools: None,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            permission_mode: "default".into(),
            max_turns: None,
            bridge: None,
            resume: false,
            ephemeral: false,
            inbox: None,
            on_rate_limit: None,
            session_id: None,
        }
    }

    /// The Claude Code session the last run used.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn set_session_id(&mut self, id: Option<String>) {
        self.session_id = id;
    }

    /// Run one user turn to completion. Ends with exactly one `Stop`.
    /// Dropping the stream kills the CLI.
    pub fn run(&mut self, prompt: String) -> BoxStream<'_, AgentEvent> {
        Box::pin(async_stream::stream! {
            let mut files = TempFiles::default();
            let bridge = match &self.bridge {
                Some((d, only)) => match McpBridge::serve(d.clone(), only.clone()).await {
                    Ok(b) => Some(b),
                    Err(e) => {
                        yield AgentEvent::Error { message: format!("claude-code: could not start the tool bridge: {e}") };
                        yield AgentEvent::Stop { reason: AgentStop::Error };
                        return;
                    }
                },
                None => None,
            };
            let args = match self.args(bridge.as_ref(), &mut files) {
                Ok(a) => a,
                Err(e) => {
                    yield AgentEvent::Error { message: format!("claude-code: {e}") };
                    yield AgentEvent::Stop { reason: AgentStop::Error };
                    return;
                }
            };
            let mut child = match spawn(&self.bin, &args, &self.cwd) {
                Ok(c) => c,
                Err(e) => {
                    yield AgentEvent::Error { message: e };
                    yield AgentEvent::Stop { reason: AgentStop::Error };
                    return;
                }
            };

            let mut stdin = child.stdin.take();
            let stdout = child.stdout.take().expect("piped stdout");
            let stderr = child.stderr.take().expect("piped stderr");
            let stderr_tail = tokio::spawn(async move {
                let mut buf = String::new();
                let _ = BufReader::new(stderr).read_to_string(&mut buf).await;
                buf
            });

            if let Some(w) = stdin.as_mut() {
                if w.write_all(user_line(&prompt).as_bytes()).await.is_err() {
                    stdin = None;
                }
            }

            let mut lines = BufReader::new(stdout).lines();
            let mut result: Option<RunResult> = None;
            while let Ok(Some(line)) = lines.next_line().await {
                // Forward queued manager messages while the input is still
                // open. Claude Code folds them into the running session.
                if let (Some(w), Some(inbox)) = (stdin.as_mut(), &self.inbox) {
                    let queued: Vec<String> = std::mem::take(&mut *inbox.lock().unwrap_or_else(|e| e.into_inner()));
                    for msg in queued {
                        let _ = w.write_all(user_line(&msg).as_bytes()).await;
                    }
                }
                let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
                for item in translate(&v) {
                    match item {
                        Item::Event(ev) => yield ev,
                        Item::Session(id) => self.session_id = Some(id),
                        Item::RateLimited(secs) => {
                            if let Some(cb) = &self.on_rate_limit { cb(secs); }
                        }
                        Item::Result(r) => {
                            if let Some(id) = &r.session_id { self.session_id = Some(id.clone()); }
                            yield AgentEvent::Usage { usage: r.usage };
                            result = Some(r);
                            // Closing input tells the CLI to exit once anything
                            // already queued has run.
                            stdin = None;
                        }
                    }
                }
            }
            drop(stdin);
            let status = child.wait().await;
            let stderr = stderr_tail.await.unwrap_or_default();

            match result {
                Some(r) if r.stop == AgentStop::Error => {
                    yield AgentEvent::Error { message: format!("claude-code: {}", r.message) };
                    yield AgentEvent::Stop { reason: AgentStop::Error };
                }
                Some(r) => yield AgentEvent::Stop { reason: r.stop },
                None => {
                    let tail: String = stderr.trim().chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect();
                    yield AgentEvent::Error {
                        message: format!(
                            "claude-code exited ({}) without a result. Is Claude Code installed and signed in? Run `claude` once to log in.\n{tail}",
                            status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string()),
                        ),
                    };
                    yield AgentEvent::Stop { reason: AgentStop::Error };
                }
            }
            drop(bridge);
        })
    }

    fn args(
        &self,
        bridge: Option<&McpBridge>,
        files: &mut TempFiles,
    ) -> std::io::Result<Vec<String>> {
        let mut a: Vec<String> = [
            "-p",
            "--verbose",
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--include-partial-messages",
            // Nobody is there to answer a permission prompt; without this the
            // CLI would ask the stdin "host" and wait forever.
            "--permission-prompts",
            "none",
            "--permission-mode",
        ]
        .map(String::from)
        .into();
        a.push(self.permission_mode.clone());
        if !self.model.is_empty() {
            a.extend(["--model".into(), self.model.clone()]);
        }
        // Files rather than arguments: role prompts outgrow the Windows
        // command-line limit, and JSON does not survive `claude.cmd` quoting.
        if let Some(s) = &self.system {
            a.extend(["--system-prompt-file".into(), files.write("system.md", s)?]);
        }
        if let Some(s) = &self.append_system {
            a.extend([
                "--append-system-prompt-file".into(),
                files.write("append.md", s)?,
            ]);
        }
        if let Some(t) = &self.tools {
            a.extend(["--tools".into(), t.clone()]);
        }
        let mut allowed = self.allowed_tools.clone();
        if let Some(b) = bridge {
            let cfg = json!({"mcpServers": {BRIDGE_NAME: {
                "type": "http",
                "url": b.url,
                "headers": {"Authorization": format!("Bearer {}", b.token)},
            }}});
            a.extend([
                "--mcp-config".into(),
                files.write("mcp.json", &cfg.to_string())?,
            ]);
            a.push("--strict-mcp-config".into());
            allowed.push(format!("mcp__{BRIDGE_NAME}"));
        }
        if !allowed.is_empty() {
            a.push("--allowedTools".into());
            a.push(allowed.join(","));
        }
        if !self.disallowed_tools.is_empty() {
            a.push("--disallowedTools".into());
            a.push(self.disallowed_tools.join(","));
        }
        if let Some(n) = self.max_turns {
            a.extend(["--max-turns".into(), n.to_string()]);
        }
        if self.ephemeral {
            a.push("--no-session-persistence".into());
        } else if let (true, Some(id)) = (self.resume, &self.session_id) {
            a.extend(["--resume".into(), id.clone()]);
        }
        Ok(a)
    }
}

fn spawn(
    bin: &str,
    args: &[String],
    cwd: &std::path::Path,
) -> Result<tokio::process::Child, String> {
    let make = |program: &str| {
        let mut c = tokio::process::Command::new(program);
        c.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The point is the user's subscription. An API key in the
            // environment (set for Wingman's `anthropic` provider) would
            // silently take precedence and bill the API instead.
            .env_remove("ANTHROPIC_API_KEY")
            // Tool calls like `run_acceptance` run a build; don't let the
            // CLI's MCP timeout fail them.
            .env("MCP_TOOL_TIMEOUT", "3600000")
            .kill_on_drop(true);
        c.spawn()
    };
    match make(bin) {
        Ok(c) => Ok(c),
        // npm installs a `claude.cmd` shim, which Windows won't find by bare name.
        Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::NotFound => {
            make(&format!("{bin}.cmd")).map_err(|_| not_found(bin))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(not_found(bin)),
        Err(e) => Err(format!("claude-code: could not start `{bin}`: {e}")),
    }
}

fn not_found(bin: &str) -> String {
    format!(
        "claude-code: `{bin}` not found. Install Claude Code (https://claude.com/claude-code), \
         run `claude` once to sign in, or point WINGMAN_CLAUDE_BIN at it."
    )
}

fn user_line(text: &str) -> String {
    let mut s = json!({"type": "user", "message": {"role": "user", "content": text}}).to_string();
    s.push('\n');
    s
}

#[derive(Debug, Clone)]
struct RunResult {
    stop: AgentStop,
    message: String,
    usage: Usage,
    session_id: Option<String>,
}

#[derive(Debug)]
enum Item {
    Event(AgentEvent),
    Session(String),
    RateLimited(Option<u64>),
    Result(RunResult),
}

/// Translate one stream-json line. Pure, so the mapping is testable without
/// the CLI. Subagent traffic (`parent_tool_use_id` set) is dropped: the
/// caller sees the subagent's tool call and its result, not its inner turns.
fn translate(v: &Value) -> Vec<Item> {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let top_level = v.get("parent_tool_use_id").is_none_or(Value::is_null);
    let mut out = Vec::new();
    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "system" if s(v, "subtype") == "init" => out.push(Item::Session(s(v, "session_id"))),
        "stream_event" if top_level => {
            let ev = &v["event"];
            match ev.get("type").and_then(Value::as_str).unwrap_or("") {
                "content_block_delta" => {
                    let d = &ev["delta"];
                    match d.get("type").and_then(Value::as_str).unwrap_or("") {
                        "text_delta" if !s(d, "text").is_empty() => {
                            out.push(Item::Event(AgentEvent::TextDelta { text: s(d, "text") }))
                        }
                        "thinking_delta" if !s(d, "thinking").is_empty() => {
                            out.push(Item::Event(AgentEvent::ThinkingDelta {
                                text: s(d, "thinking"),
                            }))
                        }
                        _ => {}
                    }
                }
                "message_stop" => out.push(Item::Event(AgentEvent::TurnComplete)),
                _ => {}
            }
        }
        // Tool calls come from the assembled message, so the input is whole.
        "assistant" if top_level => {
            for b in v["message"]["content"].as_array().into_iter().flatten() {
                if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let name = s(b, "name");
                    out.push(Item::Event(AgentEvent::ToolStart {
                        id: s(b, "id"),
                        name: name
                            .strip_prefix(BRIDGE_PREFIX)
                            .unwrap_or(&name)
                            .to_string(),
                        input: b.get("input").cloned().unwrap_or(Value::Null),
                    }));
                }
            }
        }
        "user" if top_level => {
            for b in v["message"]["content"].as_array().into_iter().flatten() {
                if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                    let output = match b.get("content") {
                        Some(Value::String(t)) => t.clone(),
                        Some(Value::Array(parts)) => parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => String::new(),
                    };
                    out.push(Item::Event(AgentEvent::ToolResult {
                        id: s(b, "tool_use_id"),
                        output,
                        is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                    }));
                }
            }
        }
        "rate_limit_event" => {
            let info = &v["rate_limit_info"];
            if s(info, "status") == "rejected" {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let secs = info
                    .get("resetsAt")
                    .and_then(Value::as_u64)
                    .map(|t| t.saturating_sub(now));
                out.push(Item::RateLimited(secs));
            }
        }
        "result" => {
            let u = &v["usage"];
            let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
            let subtype = s(v, "subtype");
            let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let stop = match subtype.as_str() {
                "error_max_turns" => AgentStop::MaxTurns,
                "success" if !is_error => AgentStop::EndTurn,
                _ => AgentStop::Error,
            };
            let message = match v.get("result").and_then(Value::as_str) {
                Some(r) if !r.is_empty() => r.to_string(),
                _ => subtype,
            };
            out.push(Item::Result(RunResult {
                stop,
                message,
                usage: Usage {
                    input_tokens: n("input_tokens"),
                    output_tokens: n("output_tokens"),
                    cache_creation_input_tokens: n("cache_creation_input_tokens"),
                    cache_read_input_tokens: n("cache_read_input_tokens"),
                },
                session_id: v
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(String::from),
            }));
        }
        _ => {}
    }
    out
}

/// Temp files for one run, removed when the run ends.
#[derive(Default)]
struct TempFiles(Vec<PathBuf>);

impl TempFiles {
    fn write(&mut self, name: &str, body: &str) -> std::io::Result<String> {
        let path = std::env::temp_dir().join(format!(
            "wingman-cc-{}-{}-{name}",
            std::process::id(),
            random_token()
        ));
        std::fs::write(&path, body)?;
        let s = path.to_string_lossy().into_owned();
        self.0.push(path);
        Ok(s)
    }
}

impl Drop for TempFiles {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

// ponytail: std's randomly keyed SipHash as a token source. Enough to keep other
// local users off a loopback port for one run; use `rand` if this ever faces a network.
fn random_token() -> String {
    let mut h = RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    let a = h.finish();
    h.write_u64(a);
    format!("{a:016x}{:016x}", h.finish())
}

/// A loopback MCP server (streamable HTTP, JSON responses) exposing a
/// [`ToolDispatcher`] to Claude Code. Stops when dropped.
pub struct McpBridge {
    pub url: String,
    pub token: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for McpBridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl McpBridge {
    pub async fn serve(
        dispatcher: Arc<dyn ToolDispatcher>,
        only: Option<Vec<String>>,
    ) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let token = random_token();
        let auth = format!("bearer {token}");
        let task = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let (d, only, auth) = (dispatcher.clone(), only.clone(), auth.clone());
                tokio::spawn(async move {
                    let _ = serve_one(sock, d, only, &auth).await;
                });
            }
        });
        Ok(Self { url, token, task })
    }
}

/// One request per connection (`Connection: close`).
async fn serve_one(
    mut sock: tokio::net::TcpStream,
    d: Arc<dyn ToolDispatcher>,
    only: Option<Vec<String>>,
    auth: &str,
) -> std::io::Result<()> {
    const MAX: usize = 16 * 1024 * 1024;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let (head_end, len) = loop {
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (i + 4, len);
        }
        if buf.len() > MAX {
            return Ok(());
        }
    };
    if len > MAX {
        return respond(&mut sock, "413 Payload Too Large", "").await;
    }
    while buf.len() < head_end + len {
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
    if !head.starts_with("post ") {
        return respond(&mut sock, "405 Method Not Allowed", "").await;
    }
    if !head.lines().any(|l| {
        l.strip_prefix("authorization:")
            .is_some_and(|v| v.trim() == auth)
    }) {
        return respond(&mut sock, "401 Unauthorized", "").await;
    }
    let Ok(msg) = serde_json::from_slice::<Value>(&buf[head_end..head_end + len]) else {
        return respond(&mut sock, "400 Bad Request", "").await;
    };
    match handle_rpc(&msg, d.as_ref(), only.as_deref()).await {
        Some(reply) => respond(&mut sock, "200 OK", &reply.to_string()).await,
        None => respond(&mut sock, "202 Accepted", "").await,
    }
}

async fn respond(
    sock: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body.as_bytes()).await?;
    sock.shutdown().await
}

/// JSON-RPC for the subset of MCP Claude Code uses. `None` for notifications.
async fn handle_rpc(msg: &Value, d: &dyn ToolDispatcher, only: Option<&[String]>) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let allowed = |name: &str| only.is_none_or(|o| o.iter().any(|n| n == name));
    let result = match msg.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => Ok(json!({
            "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": BRIDGE_NAME, "version": env!("CARGO_PKG_VERSION")},
        })),
        "ping" => Ok(json!({})),
        "tools/list" => {
            let tools: Vec<Value> = d
                .all_specs()
                .into_iter()
                .filter(|s: &ToolSpec| allowed(&s.name))
                .map(|s| json!({"name": s.name, "description": s.description, "inputSchema": s.input_schema}))
                .collect();
            Ok(json!({"tools": tools}))
        }
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() || !allowed(name) {
                Err(format!("unknown tool: {name}"))
            } else {
                let out = d
                    .dispatch(name, params.get("arguments").cloned().unwrap_or(json!({})))
                    .await;
                Ok(
                    json!({"content": [{"type": "text", "text": out.content}], "isError": out.is_error}),
                )
            }
        }
        m => Err(format!("method not found: {m}")),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": e}}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolOutcome;

    fn items(line: &str) -> Vec<Item> {
        translate(&serde_json::from_str(line).unwrap())
    }

    /// Lines captured from `claude -p --output-format stream-json` 2.1.
    #[test]
    fn stream_json_maps_to_agent_events() {
        assert!(
            matches!(&items(r#"{"type":"system","subtype":"init","session_id":"s1","tools":[]}"#)[..], [Item::Session(s)] if s == "s1")
        );
        assert!(matches!(
            &items(r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hello"}},"parent_tool_use_id":null}"#)[..],
            [Item::Event(AgentEvent::TextDelta { text })] if text == "hello"
        ));
        // Empty thinking deltas (redacted reasoning) are noise.
        assert!(items(r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":""}},"parent_tool_use_id":null}"#).is_empty());
        assert!(matches!(
            &items(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"mcp__wingman__task_complete","input":{"summary":"x"}}]},"parent_tool_use_id":null}"#)[..],
            [Item::Event(AgentEvent::ToolStart { id, name, input })] if id == "t1" && name == "task_complete" && input["summary"] == "x"
        ));
        assert!(matches!(
            &items(r#"{"type":"user","message":{"content":[{"tool_use_id":"t1","type":"tool_result","content":[{"type":"text","text":"ok"}],"is_error":true}]},"parent_tool_use_id":null}"#)[..],
            [Item::Event(AgentEvent::ToolResult { id, output, is_error: true })] if id == "t1" && output == "ok"
        ));
        // A subagent's inner turns stay inside the subagent.
        assert!(items(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t9","name":"Read","input":{}}]},"parent_tool_use_id":"t2"}"#).is_empty());
        match &items(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"hello","session_id":"s1","usage":{"input_tokens":18,"output_tokens":379,"cache_read_input_tokens":16603}}"#,
        )[..]
        {
            [Item::Result(r)] => {
                assert_eq!(r.stop, AgentStop::EndTurn);
                assert_eq!(
                    (
                        r.usage.input_tokens,
                        r.usage.output_tokens,
                        r.usage.cache_read_input_tokens
                    ),
                    (18, 379, 16603)
                );
            }
            other => panic!("{other:?}"),
        }
        match &items(r#"{"type":"result","subtype":"error_max_turns","is_error":true,"usage":{}}"#)
            [..]
        {
            [Item::Result(r)] => assert_eq!(r.stop, AgentStop::MaxTurns),
            other => panic!("{other:?}"),
        }
        assert!(items(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1}}"#
        )
        .is_empty());
        assert!(matches!(
            &items(
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1}}"#
            )[..],
            [Item::RateLimited(Some(0))]
        ));
    }

    struct Echo;
    #[async_trait::async_trait]
    impl ToolDispatcher for Echo {
        fn specs(&self) -> Vec<ToolSpec> {
            ["task_complete", "run_shell"]
                .map(|n| ToolSpec {
                    name: n.into(),
                    description: String::new(),
                    input_schema: json!({"type": "object"}),
                })
                .into()
        }
        async fn dispatch(&self, name: &str, args: Value) -> ToolOutcome {
            ToolOutcome::ok(format!("{name}:{args}"))
        }
    }

    async fn post(url: &str, auth: Option<&str>, body: &str) -> String {
        let addr = url.trim_start_matches("http://").trim_end_matches("/mcp");
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let auth = auth
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let req = format!("POST /mcp HTTP/1.1\r\nHost: x\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn the_bridge_serves_only_allowed_tools_to_the_token_holder() {
        let b = McpBridge::serve(Arc::new(Echo), Some(vec!["task_complete".into()]))
            .await
            .unwrap();
        let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        assert!(post(&b.url, None, list).await.starts_with("HTTP/1.1 401"));
        let listed = post(&b.url, Some(&b.token), list).await;
        assert!(
            listed.contains("task_complete") && !listed.contains("run_shell"),
            "{listed}"
        );
        let call = post(&b.url, Some(&b.token), r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"task_complete","arguments":{"a":1}}}"#).await;
        assert!(call.contains(r#"task_complete:{\"a\":1}"#), "{call}");
        let denied = post(&b.url, Some(&b.token), r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run_shell","arguments":{}}}"#).await;
        assert!(denied.contains("unknown tool"), "{denied}");
        let note = post(
            &b.url,
            Some(&b.token),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await;
        assert!(note.starts_with("HTTP/1.1 202"));
    }

    #[tokio::test]
    async fn a_bridged_run_is_strict_and_allows_the_bridge() {
        let mut cc = ClaudeCode::new("sonnet", std::env::temp_dir());
        cc.allowed_tools = vec!["Bash".into()];
        cc.resume = true;
        cc.set_session_id(Some("s1".into()));
        let bridge = McpBridge {
            url: "http://127.0.0.1:1/mcp".into(),
            token: "t".into(),
            task: tokio::spawn(async {}),
        };
        let mut files = TempFiles::default();
        let a = cc.args(Some(&bridge), &mut files).unwrap();
        let after = |flag: &str| a.iter().position(|x| x == flag).map(|i| a[i + 1].as_str());
        assert_eq!(after("--allowedTools"), Some("Bash,mcp__wingman"));
        assert_eq!(after("--resume"), Some("s1"));
        assert_eq!(after("--model"), Some("sonnet"));
        assert!(a.iter().any(|x| x == "--strict-mcp-config"));
        let cfg = std::fs::read_to_string(after("--mcp-config").unwrap()).unwrap();
        assert!(cfg.contains("Bearer t"));
    }
}
