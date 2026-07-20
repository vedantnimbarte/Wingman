//! A minimal, self-contained LSP client over JSON-RPC/stdio.
//!
//! We speak the LSP wire format directly (raw JSON) rather than depending on a
//! protocol-types crate: the wire shapes we use — `textDocument/definition`,
//! `references`, `hover`, `rename`, and `publishDiagnostics` — are stable, and
//! staying in `serde_json::Value` keeps this crate immune to type-crate churn
//! and free of heavy dependencies.
//!
//! The client spawns the server, performs the `initialize`/`initialized`
//! handshake, opens documents on demand, and runs a background reader task that
//! routes responses to their waiting caller and accumulates diagnostics.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};

use crate::server::Lang;

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("failed to spawn language server `{program}`: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("language server exited or closed its pipe")]
    Closed,
    #[error("i/o error talking to language server: {0}")]
    Io(#[from] std::io::Error),
    #[error("language server returned an error: {0}")]
    Server(String),
    #[error("timed out waiting for the language server")]
    Timeout,
}

pub type Result<T> = std::result::Result<T, LspError>;

/// A source position, 0-based line and UTF-16 character offset (LSP's model).
#[derive(Debug, Clone, Copy)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// A resolved location: a file plus a 0-based start position.
#[derive(Debug, Clone)]
pub struct Location {
    pub path: PathBuf,
    pub line: u32,
    pub character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

/// One diagnostic (error/warning/…) reported by the server.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub line: u32,
    pub character: u32,
    /// 1=error, 2=warning, 3=information, 4=hint (LSP `DiagnosticSeverity`).
    pub severity: u8,
    pub message: String,
    pub source: Option<String>,
}

impl Diagnostic {
    pub fn severity_label(&self) -> &'static str {
        match self.severity {
            1 => "error",
            2 => "warning",
            3 => "info",
            4 => "hint",
            _ => "diagnostic",
        }
    }

    pub fn is_error(&self) -> bool {
        self.severity == 1
    }
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;
type Diags = Arc<Mutex<HashMap<String, Vec<Diagnostic>>>>;

/// A live connection to one language server, scoped to one project root.
pub struct LspClient {
    lang: Lang,
    root: PathBuf,
    writer: Arc<Mutex<ChildStdin>>,
    next_id: AtomicI64,
    pending: Pending,
    diagnostics: Diags,
    opened: Mutex<HashSet<String>>,
    child: Mutex<Child>,
}

impl LspClient {
    pub fn lang(&self) -> Lang {
        self.lang
    }

    /// Spawn `program args…` and perform the LSP handshake rooted at `root`.
    pub async fn start(
        root: &Path,
        program: &str,
        args: &[String],
        lang: Lang,
    ) -> Result<Arc<LspClient>> {
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| LspError::Spawn {
                program: program.to_string(),
                source,
            })?;

        let stdin = child.stdin.take().ok_or(LspError::Closed)?;
        let stdout = child.stdout.take().ok_or(LspError::Closed)?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let diagnostics: Diags = Arc::new(Mutex::new(HashMap::new()));
        let writer = Arc::new(Mutex::new(stdin));

        // Reader task: route responses to callers, collect diagnostics, and
        // answer the handful of server→client requests that would otherwise
        // stall initialization.
        {
            let pending = pending.clone();
            let diagnostics = diagnostics.clone();
            let writer = writer.clone();
            tokio::spawn(async move {
                reader_loop(stdout, pending, diagnostics, writer).await;
            });
        }

        let client = Arc::new(LspClient {
            lang,
            root: root.to_path_buf(),
            writer,
            next_id: AtomicI64::new(1),
            pending,
            diagnostics,
            opened: Mutex::new(HashSet::new()),
            child: Mutex::new(child),
        });

        client.handshake().await?;
        Ok(client)
    }

    async fn handshake(&self) -> Result<()> {
        let root_uri = path_to_uri(&self.root);
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{ "uri": root_uri, "name": "root" }],
            "clientInfo": { "name": "wingman", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": {
                "textDocument": {
                    "synchronization": { "didSave": true, "dynamicRegistration": false },
                    "definition": { "dynamicRegistration": false },
                    "references": { "dynamicRegistration": false },
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                    "rename": { "dynamicRegistration": false, "prepareSupport": false },
                    "codeAction": {
                        "dynamicRegistration": false,
                        "codeActionLiteralSupport": {
                            "codeActionKind": {
                                "valueSet": [
                                    "quickfix", "refactor", "refactor.extract",
                                    "refactor.inline", "refactor.rewrite", "source",
                                    "source.organizeImports", "source.fixAll"
                                ]
                            }
                        }
                    },
                    "publishDiagnostics": { "relatedInformation": true }
                },
                "workspace": {
                    "workspaceFolders": true,
                    "configuration": true,
                    "applyEdit": true,
                    "executeCommand": { "dynamicRegistration": false }
                }
            }
        });
        // A generous timeout: rust-analyzer can be slow to answer initialize on
        // a cold cache while it starts indexing.
        self.request("initialize", params, Duration::from_secs(30))
            .await?;
        self.notify("initialized", json!({})).await?;
        Ok(())
    }

    fn alloc_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn write_message(&self, msg: &Value) -> Result<()> {
        let body = serde_json::to_vec(msg).expect("serialize json-rpc");
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut w = self.writer.lock().await;
        w.write_all(header.as_bytes()).await?;
        w.write_all(&body).await?;
        w.flush().await?;
        Ok(())
    }

    /// Send a request and await its result, bounded by `timeout`.
    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.alloc_id();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.write_message(&msg).await?;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(resp)) => {
                if let Some(err) = resp.get("error") {
                    let m = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    return Err(LspError::Server(m.to_string()));
                }
                Ok(resp.get("result").cloned().unwrap_or(Value::Null))
            }
            Ok(Err(_)) => Err(LspError::Closed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(LspError::Timeout)
            }
        }
    }

    /// Send a notification (no response expected).
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write_message(&msg).await
    }

    /// Open a document (idempotent per uri). Required before position queries.
    pub async fn open(&self, path: &Path) -> Result<String> {
        let uri = path_to_uri(path);
        {
            let mut opened = self.opened.lock().await;
            if opened.contains(&uri) {
                return Ok(uri);
            }
            opened.insert(uri.clone());
        }
        let text = tokio::fs::read_to_string(path).await.unwrap_or_default();
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": self.lang.language_id(),
                    "version": 1,
                    "text": text,
                }
            }),
        )
        .await?;
        Ok(uri)
    }

    /// `textDocument/definition` at a 0-based position.
    pub async fn definition(&self, path: &Path, pos: Position) -> Result<Vec<Location>> {
        let uri = self.open(path).await?;
        let result = self
            .request(
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": pos.line, "character": pos.character }
                }),
                Duration::from_secs(20),
            )
            .await?;
        Ok(parse_locations(&result))
    }

    /// `textDocument/references` at a 0-based position.
    pub async fn references(
        &self,
        path: &Path,
        pos: Position,
        include_declaration: bool,
    ) -> Result<Vec<Location>> {
        let uri = self.open(path).await?;
        let result = self
            .request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": pos.line, "character": pos.character },
                    "context": { "includeDeclaration": include_declaration }
                }),
                Duration::from_secs(20),
            )
            .await?;
        Ok(parse_locations(&result))
    }

    /// `textDocument/hover` → the hover text (markdown flattened to plain).
    pub async fn hover(&self, path: &Path, pos: Position) -> Result<Option<String>> {
        let uri = self.open(path).await?;
        let result = self
            .request(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": pos.line, "character": pos.character }
                }),
                Duration::from_secs(15),
            )
            .await?;
        Ok(parse_hover(&result))
    }

    /// `textDocument/rename` → the raw `WorkspaceEdit` (as JSON), or `None` if
    /// the server declined. The caller decides whether to apply it.
    pub async fn rename(
        &self,
        path: &Path,
        pos: Position,
        new_name: &str,
    ) -> Result<Option<Value>> {
        let uri = self.open(path).await?;
        let result = self
            .request(
                "textDocument/rename",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": pos.line, "character": pos.character },
                    "newName": new_name
                }),
                Duration::from_secs(20),
            )
            .await?;
        if result.is_null() {
            Ok(None)
        } else {
            Ok(Some(result))
        }
    }

    /// `textDocument/codeAction` — available quick-fixes / refactors / source
    /// actions for `[start, end]`. Returns the raw action objects (each has a
    /// `title`, optional `kind`, optional inline `edit` (WorkspaceEdit), and/or
    /// a `command`). `only_kinds` filters by kind (e.g.
    /// `["source.organizeImports"]`, `["quickfix"]`); empty means all.
    pub async fn code_actions(
        &self,
        path: &Path,
        start: Position,
        end: Position,
        only_kinds: &[String],
    ) -> Result<Vec<Value>> {
        let uri = self.open(path).await?;
        // Reconstruct a diagnostics context from what we've accumulated so the
        // server can attach diagnostic-linked quick-fixes.
        let diags_ctx: Vec<Value> = self
            .diagnostics
            .lock()
            .await
            .get(&uri)
            .map(|ds| {
                ds.iter()
                    .map(|d| {
                        json!({
                            "range": {
                                "start": { "line": d.line, "character": d.character },
                                "end": { "line": d.line, "character": d.character }
                            },
                            "severity": d.severity,
                            "message": d.message,
                            "source": d.source,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut context = json!({ "diagnostics": diags_ctx });
        if !only_kinds.is_empty() {
            context["only"] = json!(only_kinds);
        }
        let result = self
            .request(
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": { "line": start.line, "character": start.character },
                        "end": { "line": end.line, "character": end.character }
                    },
                    "context": context
                }),
                Duration::from_secs(20),
            )
            .await?;
        Ok(match result {
            Value::Array(a) => a,
            _ => Vec::new(),
        })
    }

    /// `workspace/executeCommand` — run a server command (a code action's
    /// `command`). The server typically responds by pushing a
    /// `workspace/applyEdit` request, which the reader loop applies to disk.
    pub async fn execute_command(&self, command: &str, arguments: Value) -> Result<Value> {
        self.request(
            "workspace/executeCommand",
            json!({ "command": command, "arguments": arguments }),
            Duration::from_secs(20),
        )
        .await
    }

    /// Open `path` and wait up to `timeout` for the server to publish
    /// diagnostics for it. Diagnostics arrive asynchronously after `didOpen`,
    /// so we poll our accumulated map until an entry appears or time runs out.
    /// Returns whatever we have when the timeout elapses (possibly empty — a
    /// clean file legitimately yields no diagnostics).
    pub async fn diagnostics(&self, path: &Path, timeout: Duration) -> Result<Vec<Diagnostic>> {
        let uri = self.open(path).await?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(d) = self.diagnostics.lock().await.get(&uri) {
                return Ok(d.clone());
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(Vec::new());
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
    }

    /// Best-effort graceful shutdown.
    pub async fn shutdown(&self) {
        let _ = self
            .request("shutdown", Value::Null, Duration::from_secs(3))
            .await;
        let _ = self.notify("exit", Value::Null).await;
        let _ = self.child.lock().await.start_kill();
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // `kill_on_drop(true)` on the Child handles process teardown; nothing
        // more to do synchronously here.
    }
}

/// Background loop: parse frames off the server's stdout and dispatch them.
async fn reader_loop(
    stdout: tokio::process::ChildStdout,
    pending: Pending,
    diagnostics: Diags,
    writer: Arc<Mutex<ChildStdin>>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let msg = match read_message(&mut reader).await {
            Ok(Some(v)) => v,
            Ok(None) | Err(_) => break, // pipe closed / malformed → server gone
        };

        // Response to one of our requests?
        if msg.get("id").is_some() && (msg.get("result").is_some() || msg.get("error").is_some()) {
            if let Some(id) = msg.get("id").and_then(Value::as_i64) {
                if let Some(tx) = pending.lock().await.remove(&id) {
                    let _ = tx.send(msg);
                }
            }
            continue;
        }

        // Server→client request (has both method and id): answer minimally so
        // initialization doesn't stall.
        if let (Some(method), Some(id)) = (msg.get("method").and_then(Value::as_str), msg.get("id"))
        {
            let result = match method {
                // Each configuration item gets a null (use defaults).
                "workspace/configuration" => {
                    let n = msg
                        .get("params")
                        .and_then(|p| p.get("items"))
                        .and_then(Value::as_array)
                        .map(|a| a.len())
                        .unwrap_or(0);
                    Value::Array(vec![Value::Null; n])
                }
                // A code action's command asks us to apply an edit — do it and
                // report the result, so command-based fixes actually land.
                "workspace/applyEdit" => {
                    let edit = msg
                        .get("params")
                        .and_then(|p| p.get("edit"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    match crate::edit::apply_workspace_edit(&edit).await {
                        Ok(_) => json!({ "applied": true }),
                        Err(e) => json!({ "applied": false, "failureReason": e.to_string() }),
                    }
                }
                // Everything else we don't implement → null result is safe.
                _ => Value::Null,
            };
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            let body = serde_json::to_vec(&reply).unwrap_or_default();
            let header = format!("Content-Length: {}\r\n\r\n", body.len());
            let mut w = writer.lock().await;
            let _ = w.write_all(header.as_bytes()).await;
            let _ = w.write_all(&body).await;
            let _ = w.flush().await;
            continue;
        }

        // Notification from the server.
        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            if method == "textDocument/publishDiagnostics" {
                if let Some(params) = msg.get("params") {
                    if let Some(uri) = params.get("uri").and_then(Value::as_str) {
                        let diags = parse_diagnostics(params);
                        diagnostics.lock().await.insert(uri.to_string(), diags);
                    }
                }
            }
            // Other notifications ($/progress, window/logMessage, …) ignored.
        }
    }

    // Server is gone: unblock every waiter so callers get `Closed`, not a hang.
    let mut p = pending.lock().await;
    for (_, tx) in p.drain() {
        let _ = tx.send(json!({ "error": { "message": "language server closed" } }));
    }
}

/// Read one `Content-Length`-framed JSON message. `Ok(None)` on clean EOF.
async fn read_message<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(v) = trimmed.strip_prefix("Content-Length:") {
            content_length = v.trim().parse::<usize>().ok();
        }
    }
    let len = match content_length {
        Some(l) => l,
        None => return Ok(None), // framing violation → treat as gone
    };
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    match serde_json::from_slice::<Value>(&buf) {
        Ok(v) => Ok(Some(v)),
        Err(_) => Ok(Some(Value::Null)), // skip an unparseable frame, keep going
    }
}

// ---- response parsing (raw JSON → our small typed structs) ----------------

fn one_location(v: &Value) -> Option<Location> {
    // Either a `Location { uri, range }` or a `LocationLink { targetUri,
    // targetSelectionRange }`.
    let (uri, range) = match (v.get("uri"), v.get("targetUri")) {
        (Some(uri), _) => (uri, v.get("range")),
        (None, Some(uri)) => (
            uri,
            v.get("targetSelectionRange")
                .or_else(|| v.get("targetRange")),
        ),
        (None, None) => return None,
    };
    let uri = uri.as_str()?;
    let path = uri_to_path(uri)?;
    let range = range?;
    let start = range.get("start")?;
    let end = range.get("end").unwrap_or(start);
    Some(Location {
        path,
        line: start.get("line").and_then(Value::as_u64).unwrap_or(0) as u32,
        character: start.get("character").and_then(Value::as_u64).unwrap_or(0) as u32,
        end_line: end.get("line").and_then(Value::as_u64).unwrap_or(0) as u32,
        end_character: end.get("character").and_then(Value::as_u64).unwrap_or(0) as u32,
    })
}

fn parse_locations(result: &Value) -> Vec<Location> {
    match result {
        Value::Null => Vec::new(),
        Value::Array(arr) => arr.iter().filter_map(one_location).collect(),
        // A single Location object.
        v => one_location(v).into_iter().collect(),
    }
}

fn parse_diagnostics(params: &Value) -> Vec<Diagnostic> {
    let Some(arr) = params.get("diagnostics").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|d| {
            let start = d.get("range")?.get("start")?;
            Some(Diagnostic {
                line: start.get("line").and_then(Value::as_u64).unwrap_or(0) as u32,
                character: start.get("character").and_then(Value::as_u64).unwrap_or(0) as u32,
                severity: d.get("severity").and_then(Value::as_u64).unwrap_or(1) as u8,
                message: d
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                source: d.get("source").and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect()
}

fn parse_hover(result: &Value) -> Option<String> {
    let contents = result.get("contents")?;
    let text = match contents {
        Value::String(s) => s.clone(),
        // MarkupContent { kind, value }
        Value::Object(o) => o.get("value").and_then(Value::as_str)?.to_string(),
        // MarkedString[] — array of strings or { language, value }.
        Value::Array(arr) => arr
            .iter()
            .filter_map(|m| match m {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("value").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// ---- path <-> file URI ----------------------------------------------------

/// Convert a filesystem path to a `file://` URI with the minimal
/// percent-encoding LSP servers expect. Absolute paths only in practice.
pub fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let mut encoded = String::with_capacity(s.len() + 16);
    for ch in s.chars() {
        match ch {
            // Unreserved per RFC 3986 + path punctuation servers accept raw.
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '.' | '_' | '~' | '/' | ':' => {
                encoded.push(ch)
            }
            _ => {
                let mut buf = [0u8; 4];
                for b in ch.encode_utf8(&mut buf).bytes() {
                    encoded.push('%');
                    encoded.push_str(&format!("{b:02X}"));
                }
            }
        }
    }
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        // Windows drive path like C:/… → file:///C:/…
        format!("file:///{encoded}")
    }
}

/// Convert a `file://` URI back to a filesystem path, undoing percent-encoding.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // Drop an empty authority: file:///C:/x → /C:/x ; file:///home → /home.
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let decoded = percent_decode(rest);
    // On Windows the leading segment is a drive (C:/…); on unix restore root /.
    if decoded.len() >= 2 && decoded.as_bytes()[1] == b':' {
        Some(PathBuf::from(
            decoded.replace('/', std::path::MAIN_SEPARATOR_STR),
        ))
    } else {
        Some(PathBuf::from(format!("/{decoded}")))
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip_unix() {
        let uri = path_to_uri(Path::new("/home/me/a b/x.rs"));
        assert!(uri.starts_with("file:///home/me/"));
        assert!(uri.contains("%20")); // space encoded
        let back = uri_to_path(&uri).unwrap();
        assert_eq!(
            back.to_string_lossy().replace('\\', "/"),
            "/home/me/a b/x.rs"
        );
    }

    #[test]
    fn uri_windows_drive() {
        let uri = path_to_uri(Path::new(r"C:\proj\src\main.rs"));
        assert_eq!(uri, "file:///C:/proj/src/main.rs");
        let back = uri_to_path(&uri).unwrap();
        assert!(back.to_string_lossy().contains("proj"));
        assert!(back.to_string_lossy().starts_with("C:"));
    }

    #[test]
    fn parse_single_and_array_locations() {
        let single = json!({ "uri": "file:///x/y.rs", "range": {
            "start": { "line": 3, "character": 2 }, "end": { "line": 3, "character": 8 } } });
        assert_eq!(parse_locations(&single).len(), 1);
        let arr = json!([single.clone(), single]);
        assert_eq!(parse_locations(&arr).len(), 2);
        assert!(parse_locations(&Value::Null).is_empty());
    }

    #[test]
    fn parse_location_link_shape() {
        let link = json!([{ "targetUri": "file:///x/y.rs", "targetSelectionRange": {
            "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 4 } } }]);
        let locs = parse_locations(&link);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].line, 1);
    }

    #[test]
    fn parse_hover_variants() {
        assert_eq!(
            parse_hover(&json!({ "contents": "hello" })).as_deref(),
            Some("hello")
        );
        assert_eq!(
            parse_hover(&json!({ "contents": { "kind": "markdown", "value": "`fn`" } })).as_deref(),
            Some("`fn`")
        );
        assert!(parse_hover(&json!({ "contents": "" })).is_none());
    }

    #[test]
    fn parse_diagnostics_extracts_severity_and_message() {
        let params = json!({ "uri": "file:///x.rs", "diagnostics": [
            { "range": { "start": { "line": 9, "character": 4 }, "end": { "line": 9, "character": 10 } },
              "severity": 1, "message": "mismatched types", "source": "rustc" }
        ]});
        let d = parse_diagnostics(&params);
        assert_eq!(d.len(), 1);
        assert!(d[0].is_error());
        assert_eq!(d[0].line, 9);
        assert_eq!(d[0].message, "mismatched types");
        assert_eq!(d[0].source.as_deref(), Some("rustc"));
    }
}
