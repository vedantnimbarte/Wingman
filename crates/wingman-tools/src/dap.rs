//! Debug Adapter Protocol: the agent asks the debugger.
//!
//! `wingman-lsp` asks the compiler what the code *means*; this asks the
//! runtime what it *did*. One adapter process per session — lldb-dap,
//! debugpy, or Delve — driven over DAP, which uses LSP's `Content-Length`
//! framing, so the frame reader and writer are `wingman_lsp::client`'s rather
//! than a second copy.
//!
//! Three properties, all borrowed from background jobs ([`crate::jobs`]):
//!
//! - **The same gate as `run_shell`.** A debug session runs a program. The
//!   adapter command goes through `run_shell`'s own preparation (mode,
//!   denylist, sandbox, credential scrub), and the debuggee's command line is
//!   checked against the denylist as well.
//! - **Killed with its tree.** The adapter is spawned under
//!   [`crate::child_process`] and the debuggee is its child, so stopping a
//!   session — or the table dropping at session end — reaps both.
//! - **Bounded output.** Program output lands in a [`Tail`]; stack frames,
//!   variables, and values are capped per call.
//!
//! No adapter installed is a message naming what to install, not an error.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::oneshot;
use tokio::time::Instant;
use wingman_lsp::client::{read_message, write_message};

use crate::child_process::Supervisor;
use crate::jobs::Tail;

const MAX_FRAMES: usize = 20;
const MAX_VARS: usize = 40;
const MAX_VALUE_CHARS: usize = 200;
const MAX_EVAL_CHARS: usize = 4000;
/// Ordinary requests: stack, scopes, variables, stepping.
const SHORT: Duration = Duration::from_secs(10);
/// `launch` can load a large binary's symbols before it answers.
const LAUNCH: Duration = Duration::from_secs(60);

/// Which family of adapter a program needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugLang {
    /// Rust, C, C++ — anything LLDB debugs.
    Native,
    Python,
    Go,
}

impl DebugLang {
    pub const ALL: [DebugLang; 3] = [Self::Native, Self::Python, Self::Go];

    pub fn label(self) -> &'static str {
        match self {
            Self::Native => "rust/c/c++",
            Self::Python => "python",
            Self::Go => "go",
        }
    }

    /// What to install when [`Adapter::detect`] finds nothing.
    pub fn install_hint(self) -> &'static str {
        match self {
            Self::Native => {
                "lldb-dap (ships with LLVM 18+; `lldb-vscode` in older LLVM) or CodeLLDB's `codelldb`"
            }
            Self::Python => "debugpy (`pip install debugpy`) for the `python3`/`python` on PATH",
            Self::Go => "Delve (`go install github.com/go-delve/delve/cmd/dlv@latest`)",
        }
    }

    /// An explicit `language` wins; otherwise infer from the program. A
    /// Python `module` means Python; anything that is not `.py`/`.go` is
    /// taken to be a native binary.
    pub fn pick(
        language: Option<&str>,
        program: Option<&str>,
        module: bool,
    ) -> Result<Self, String> {
        if let Some(l) = language {
            return match l.to_ascii_lowercase().as_str() {
                "rust" | "c" | "cpp" | "c++" | "native" => Ok(Self::Native),
                "python" | "py" => Ok(Self::Python),
                "go" => Ok(Self::Go),
                other => Err(format!(
                    "no debug adapter for `{other}` (supported: rust, c, cpp, python, go)"
                )),
            };
        }
        if module {
            return Ok(Self::Python);
        }
        let ext = program
            .and_then(|p| Path::new(p).extension())
            .and_then(|e| e.to_str())
            .unwrap_or("");
        Ok(match ext {
            "py" => Self::Python,
            "go" => Self::Go,
            _ => Self::Native,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// DAP over the adapter's stdin/stdout.
    Stdio,
    /// The adapter listens on a local port we choose (Delve, CodeLLDB).
    Tcp,
}

/// One way to launch a debug adapter.
#[derive(Debug, Clone, Copy)]
pub struct Adapter {
    pub program: &'static str,
    /// `{port}` is replaced with the chosen port for [`Transport::Tcp`].
    pub args: &'static [&'static str],
    pub transport: Transport,
    /// Extra arguments that must exit 0 for the adapter to count as
    /// installed: `python` on PATH says nothing about whether debugpy is.
    probe: Option<&'static [&'static str]>,
}

const fn stdio(program: &'static str, args: &'static [&'static str]) -> Adapter {
    Adapter {
        program,
        args,
        transport: Transport::Stdio,
        probe: None,
    }
}

const NATIVE: &[Adapter] = &[
    stdio("lldb-dap", &[]),
    stdio("lldb-vscode", &[]),
    Adapter {
        program: "codelldb",
        args: &["--port", "{port}"],
        transport: Transport::Tcp,
        probe: None,
    },
];
const DEBUGPY: &[&str] = &["-m", "debugpy.adapter"];
const HAS_DEBUGPY: Option<&[&str]> = Some(&["-c", "import debugpy"]);
const PYTHON: &[Adapter] = &[
    Adapter {
        probe: HAS_DEBUGPY,
        ..stdio("python3", DEBUGPY)
    },
    Adapter {
        probe: HAS_DEBUGPY,
        ..stdio("python", DEBUGPY)
    },
];
const GO: &[Adapter] = &[Adapter {
    program: "dlv",
    args: &["dap", "--listen", "127.0.0.1:{port}"],
    transport: Transport::Tcp,
    probe: None,
}];

impl Adapter {
    pub fn candidates(lang: DebugLang) -> &'static [Adapter] {
        match lang {
            DebugLang::Native => NATIVE,
            DebugLang::Python => PYTHON,
            DebugLang::Go => GO,
        }
    }

    /// The first installed adapter for `lang`, in preference order. Blocking:
    /// the debugpy probe starts a Python.
    pub fn detect(lang: DebugLang) -> Option<Adapter> {
        Self::candidates(lang).iter().copied().find(|a| {
            wingman_lsp::server::which_on_path(a.program).is_some()
                && a.probe.is_none_or(|probe| {
                    std::process::Command::new(a.program)
                        .args(probe)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success())
                })
        })
    }

    /// The shell command line that starts this adapter.
    pub fn command_line(&self, port: Option<u16>) -> String {
        let port = port.map(|p| p.to_string()).unwrap_or_default();
        std::iter::once(self.program.to_string())
            .chain(self.args.iter().map(|a| a.replace("{port}", &port)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A line breakpoint, 1-based.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Breakpoint {
    pub line: u32,
    #[serde(default)]
    pub condition: Option<String>,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;
type Writer = Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;

/// What the adapter has told us, as opposed to what we asked.
#[derive(Default)]
struct Events {
    initialized: bool,
    /// Body of the latest `stopped` event; cleared when execution resumes.
    stopped: Option<Value>,
    /// Set once the program exits or the adapter goes away.
    ended: Option<String>,
    /// Program output not yet shown to the agent.
    output: Tail,
}

/// A live connection to one debug adapter.
pub struct DapClient {
    writer: Writer,
    seq: AtomicI64,
    pending: Pending,
    events: Arc<Mutex<Events>>,
}

impl DapClient {
    /// Speak DAP over an open byte stream: a spawned adapter's pipes, a TCP
    /// socket, or an in-process fake in tests.
    pub fn connect(
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Arc<DapClient> {
        let pending: Pending = Arc::default();
        let events: Arc<Mutex<Events>> = Arc::default();
        let writer: Writer = Arc::new(tokio::sync::Mutex::new(Box::new(writer)));
        tokio::spawn(reader_loop(
            reader,
            pending.clone(),
            events.clone(),
            writer.clone(),
        ));
        Arc::new(DapClient {
            writer,
            seq: AtomicI64::new(1),
            pending,
            events,
        })
    }

    async fn send(
        &self,
        command: &str,
        arguments: Value,
    ) -> Result<oneshot::Receiver<Value>, String> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(seq, tx);
        // The reader marks the session ended *before* it drops the waiters,
        // so checking after inserting cannot miss an adapter that just died.
        if self.events.lock().unwrap().ended.is_some() {
            self.pending.lock().unwrap().remove(&seq);
            return Err(self.gone());
        }
        let msg =
            json!({ "seq": seq, "type": "request", "command": command, "arguments": arguments });
        write_message(&mut *self.writer.lock().await, &msg)
            .await
            .map_err(|e| format!("the debug adapter's pipe closed: {e}"))?;
        Ok(rx)
    }

    /// Send a request and wait for its response body.
    pub async fn request(
        &self,
        command: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let rx = self.send(command, arguments).await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(resp)) => check(command, resp),
            Ok(Err(_)) => Err(self.gone()),
            Err(_) => Err(format!(
                "the debug adapter did not answer `{command}` within {}s",
                timeout.as_secs()
            )),
        }
    }

    fn gone(&self) -> String {
        let ended = self.events.lock().unwrap().ended.clone();
        ended.unwrap_or_else(|| "the debug adapter exited".into())
    }

    /// `initialize` → `launch` → breakpoints → `configurationDone`. Returns
    /// the breakpoint report. Does not wait for the program to stop.
    pub async fn launch(
        &self,
        adapter_id: &str,
        launch_args: Value,
        breakpoints: &[(PathBuf, Vec<Breakpoint>)],
    ) -> Result<String, String> {
        let caps = self
            .request(
                "initialize",
                json!({
                    "clientID": "wingman",
                    "clientName": "wingman",
                    "adapterID": adapter_id,
                    "linesStartAt1": true,
                    "columnsStartAt1": true,
                    "pathFormat": "path",
                    "supportsVariableType": true,
                    "supportsRunInTerminalRequest": false,
                }),
                SHORT,
            )
            .await?;
        // Adapters commonly answer `launch` only after `configurationDone`,
        // so it cannot be awaited here: wait for `initialized` instead, while
        // watching for a launch that fails outright (no such program).
        let mut launch = Some(self.send("launch", launch_args).await?);
        let deadline = Instant::now() + LAUNCH;
        while !self.events.lock().unwrap().initialized {
            if let Some(rx) = launch.as_mut() {
                match rx.try_recv() {
                    Ok(resp) => {
                        check("launch", resp)?;
                        launch = None;
                    }
                    Err(oneshot::error::TryRecvError::Closed) => return Err(self.gone()),
                    Err(oneshot::error::TryRecvError::Empty) => {}
                }
            }
            if Instant::now() >= deadline {
                return Err("the debug adapter never became ready (no `initialized` event)".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut report = Vec::new();
        for (path, bps) in breakpoints {
            report.push(self.set_breakpoints(path, bps).await?);
        }
        if caps["supportsConfigurationDoneRequest"].as_bool() == Some(true) {
            self.request("configurationDone", json!({}), SHORT).await?;
        }
        if let Some(rx) = launch {
            match tokio::time::timeout(LAUNCH, rx).await {
                Ok(Ok(resp)) => {
                    check("launch", resp)?;
                }
                Ok(Err(_)) => return Err(self.gone()),
                Err(_) => return Err("the debug adapter did not answer `launch`".into()),
            }
        }
        Ok(report.join("\n"))
    }

    /// Replace every breakpoint in `path` with `bps` (DAP's own semantics:
    /// an empty list clears the file).
    pub async fn set_breakpoints(&self, path: &Path, bps: &[Breakpoint]) -> Result<String, String> {
        let wanted: Vec<Value> = bps
            .iter()
            .map(|b| {
                let mut v = json!({ "line": b.line });
                if let Some(c) = &b.condition {
                    v["condition"] = json!(c);
                }
                v
            })
            .collect();
        let body = self
            .request(
                "setBreakpoints",
                json!({ "source": { "path": path }, "breakpoints": wanted }),
                SHORT,
            )
            .await?;
        if bps.is_empty() {
            return Ok(format!("cleared breakpoints in {}", path.display()));
        }
        let got = body["breakpoints"].as_array().cloned().unwrap_or_default();
        Ok(bps
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let r = got.get(i).unwrap_or(&Value::Null);
                let line = r["line"].as_u64().unwrap_or(b.line.into());
                // Unverified usually means "not loaded yet", not "wrong".
                let status = if r["verified"].as_bool() == Some(true) {
                    "verified"
                } else {
                    "pending"
                };
                let why = r["message"]
                    .as_str()
                    .map(|m| format!(" ({m})"))
                    .unwrap_or_default();
                format!("breakpoint {}:{line} {status}{why}", path.display())
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// `continue`, or step `over` / `in` / `out`, then wait up to `wait` and
    /// report where it ended up.
    pub async fn resume(&self, action: &str, wait: Duration) -> Result<String, String> {
        let command = match action {
            "continue" => "continue",
            "over" => "next",
            "in" => "stepIn",
            "out" => "stepOut",
            other => {
                return Err(format!(
                    "unknown action `{other}` (continue, over, in, out)"
                ))
            }
        };
        let thread = self.stopped_thread().await?;
        // Cleared before sending, so any `stopped` that arrives is the new one.
        self.events.lock().unwrap().stopped = None;
        self.request(command, json!({ "threadId": thread }), SHORT)
            .await?;
        Ok(self.state(wait).await)
    }

    async fn stopped_thread(&self) -> Result<i64, String> {
        let (stopped, ended) = {
            let ev = self.events.lock().unwrap();
            (ev.stopped.clone(), ev.ended.clone())
        };
        if let Some(e) = ended {
            return Err(format!("{e}; the session is over"));
        }
        let Some(stopped) = stopped else {
            return Err(
                "the program is running, not stopped — call debug_state to wait for it".into(),
            );
        };
        if let Some(t) = stopped["threadId"].as_i64() {
            return Ok(t);
        }
        let threads = self.request("threads", json!({}), SHORT).await?;
        threads["threads"][0]["id"]
            .as_i64()
            .ok_or_else(|| "the debug adapter reported no threads".into())
    }

    /// Wait up to `wait` for the program to stop or end, then describe it:
    /// stop reason, stack, the top frame's locals, and output since the last
    /// call.
    pub async fn state(&self, wait: Duration) -> String {
        let deadline = Instant::now() + wait;
        loop {
            {
                let ev = self.events.lock().unwrap();
                if ev.stopped.is_some() || ev.ended.is_some() {
                    break;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            // ponytail: 50ms poll; a watch channel if stop latency ever matters.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let (stopped, ended, output) = {
            let mut ev = self.events.lock().unwrap();
            (
                ev.stopped.clone(),
                ev.ended.clone(),
                std::mem::take(&mut ev.output),
            )
        };
        let mut out = if let Some(e) = ended {
            format!("{e}\n")
        } else if let Some(s) = stopped {
            self.describe_stop(&s).await.unwrap_or_else(|e| {
                format!(
                    "stopped ({}), but reading the stack failed: {e}\n",
                    s["reason"]
                )
            })
        } else {
            format!(
                "still running after {}s — call debug_state again to keep waiting, or debug_stop\n",
                wait.as_secs()
            )
        };
        let text = output.render();
        if !text.is_empty() {
            out.push_str("[program output since last check]\n");
            out.push_str(&text);
        }
        out
    }

    async fn describe_stop(&self, stopped: &Value) -> Result<String, String> {
        let thread = self.stopped_thread().await?;
        let mut out = format!(
            "stopped: {}",
            stopped["reason"].as_str().unwrap_or("unknown")
        );
        if let Some(d) = stopped["description"].as_str().or(stopped["text"].as_str()) {
            out.push_str(&format!(" — {d}"));
        }
        out.push_str(&format!(" (thread {thread})\n"));
        let trace = self
            .request(
                "stackTrace",
                json!({ "threadId": thread, "startFrame": 0, "levels": MAX_FRAMES }),
                SHORT,
            )
            .await?;
        let frames = trace["stackFrames"].as_array().cloned().unwrap_or_default();
        for f in &frames {
            let source = f["source"]["path"]
                .as_str()
                .or(f["source"]["name"].as_str())
                .unwrap_or("<no source>");
            out.push_str(&format!(
                "  frame {}: {}  {source}:{}\n",
                f["id"],
                f["name"].as_str().unwrap_or("?"),
                f["line"]
            ));
        }
        if let Some(total) = trace["totalFrames"].as_u64() {
            if total as usize > frames.len() {
                out.push_str(&format!(
                    "  … {} more frames\n",
                    total as usize - frames.len()
                ));
            }
        }
        if let Some(top) = frames.first().and_then(|f| f["id"].as_i64()) {
            out.push_str(&self.locals(top).await?);
        }
        Ok(out)
    }

    async fn locals(&self, frame: i64) -> Result<String, String> {
        let scopes = self
            .request("scopes", json!({ "frameId": frame }), SHORT)
            .await?;
        let mut out = String::new();
        for scope in scopes["scopes"].as_array().into_iter().flatten() {
            // Globals and registers can run to thousands of entries; adapters
            // flag those scopes as expensive.
            if scope["expensive"].as_bool() == Some(true) {
                continue;
            }
            let Some(reference) = scope["variablesReference"].as_i64().filter(|r| *r > 0) else {
                continue;
            };
            let vars = self
                .request(
                    "variables",
                    json!({ "variablesReference": reference }),
                    SHORT,
                )
                .await?;
            let vars = vars["variables"].as_array().cloned().unwrap_or_default();
            out.push_str(&format!("{}:\n", scope["name"].as_str().unwrap_or("scope")));
            for v in vars.iter().take(MAX_VARS) {
                let ty = v["type"]
                    .as_str()
                    .filter(|t| !t.is_empty())
                    .map(|t| format!(" ({t})"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  {} = {}{ty}\n",
                    v["name"].as_str().unwrap_or("?"),
                    clip(v["value"].as_str().unwrap_or(""), MAX_VALUE_CHARS)
                ));
            }
            if vars.len() > MAX_VARS {
                out.push_str(&format!("  … {} more\n", vars.len() - MAX_VARS));
            }
        }
        Ok(out)
    }

    /// Evaluate `expression` in `frame`, defaulting to the top frame of the
    /// stopped thread (or global context while running).
    pub async fn evaluate(&self, expression: &str, frame: Option<i64>) -> Result<String, String> {
        let frame = match frame {
            Some(f) => Some(f),
            None if self.events.lock().unwrap().stopped.is_some() => {
                let thread = self.stopped_thread().await?;
                let trace = self
                    .request(
                        "stackTrace",
                        json!({ "threadId": thread, "startFrame": 0, "levels": 1 }),
                        SHORT,
                    )
                    .await?;
                trace["stackFrames"][0]["id"].as_i64()
            }
            None => None,
        };
        let mut args = json!({ "expression": expression, "context": "repl" });
        if let Some(f) = frame {
            args["frameId"] = json!(f);
        }
        // An expression can call into the program, which can be slow.
        let body = self
            .request("evaluate", args, Duration::from_secs(30))
            .await?;
        let ty = body["type"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(|t| format!(" ({t})"))
            .unwrap_or_default();
        Ok(format!(
            "{}{ty}",
            clip(body["result"].as_str().unwrap_or(""), MAX_EVAL_CHARS)
        ))
    }

    /// Ask the adapter to end the session and the debuggee. Best-effort: the
    /// caller kills the process tree regardless.
    pub async fn disconnect(&self) {
        let _ = self
            .request(
                "disconnect",
                json!({ "terminateDebuggee": true }),
                Duration::from_secs(3),
            )
            .await;
    }
}

/// A response's body, or its failure as a readable message.
fn check(command: &str, resp: Value) -> Result<Value, String> {
    if resp["success"].as_bool() == Some(true) {
        return Ok(resp.get("body").cloned().unwrap_or(Value::Null));
    }
    let why = resp["message"]
        .as_str()
        .or(resp["body"]["error"]["format"].as_str())
        .unwrap_or("no reason given");
    Err(format!("`{command}` failed: {why}"))
}

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

async fn reader_loop(
    reader: impl AsyncRead + Unpin,
    pending: Pending,
    events: Arc<Mutex<Events>>,
    writer: Writer,
) {
    let mut reader = BufReader::new(reader);
    while let Ok(Some(msg)) = read_message(&mut reader).await {
        match msg["type"].as_str() {
            Some("response") => {
                let waiter = msg["request_seq"]
                    .as_i64()
                    .and_then(|s| pending.lock().unwrap().remove(&s));
                if let Some(tx) = waiter {
                    let _ = tx.send(msg);
                }
            }
            Some("event") => on_event(&mut events.lock().unwrap(), &msg),
            // Reverse requests (`runInTerminal`, `startDebugging`). We
            // advertise neither, so refuse rather than leave the adapter
            // waiting on an answer.
            Some("request") => {
                let reply = json!({
                    "seq": 0, "type": "response", "request_seq": msg["seq"],
                    "success": false, "command": msg["command"],
                    "message": "not supported by wingman",
                });
                let _ = write_message(&mut *writer.lock().await, &reply).await;
            }
            // `Null` from an unparseable frame: skip it.
            _ => {}
        }
    }
    events
        .lock()
        .unwrap()
        .ended
        .get_or_insert_with(|| "the debug adapter exited".into());
    // Dropping the senders wakes every waiter with an error instead of a hang.
    pending.lock().unwrap().clear();
}

fn on_event(ev: &mut Events, msg: &Value) {
    let body = &msg["body"];
    match msg["event"].as_str().unwrap_or("") {
        "initialized" => ev.initialized = true,
        "stopped" => ev.stopped = Some(body.clone()),
        "continued" => ev.stopped = None,
        "exited" => {
            ev.stopped = None;
            ev.ended = Some(format!("program exited with code {}", body["exitCode"]));
        }
        "terminated" => {
            ev.stopped = None;
            ev.ended
                .get_or_insert_with(|| "the debug session terminated".into());
        }
        "output" if body["category"] != "telemetry" => {
            if let Some(text) = body["output"].as_str() {
                ev.output.push(text.as_bytes());
            }
        }
        _ => {}
    }
}

struct Session {
    client: Arc<DapClient>,
    /// Dropping this kills the adapter and the debuggee under it. `None` for
    /// a session over a stream we did not spawn (tests).
    supervisor: Mutex<Option<Supervisor>>,
}

/// The session's debug sessions. Shared through [`crate::ToolCtx`] like
/// [`crate::jobs::JobTable`], and for the same reason: a session a subagent
/// started still belongs to the agent session.
#[derive(Default)]
pub struct DebugSessions {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    next: AtomicU64,
}

impl DebugSessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Track a launched session and return its id.
    pub fn insert(&self, client: Arc<DapClient>, supervisor: Option<Supervisor>) -> String {
        let id = format!("dbg-{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
        let session = Arc::new(Session {
            client,
            supervisor: Mutex::new(supervisor),
        });
        self.sessions.lock().unwrap().insert(id.clone(), session);
        id
    }

    pub fn get(&self, id: &str) -> Result<Arc<DapClient>, String> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .map(|s| s.client.clone())
            .ok_or_else(|| format!("no such debug session: {id} (start one with debug_start)"))
    }

    /// End a session and kill its process tree.
    pub async fn stop(&self, id: &str) -> Result<(), String> {
        let session = self
            .sessions
            .lock()
            .unwrap()
            .remove(id)
            .ok_or_else(|| format!("no such debug session: {id}"))?;
        session.client.disconnect().await;
        drop(session.supervisor.lock().unwrap().take());
        Ok(())
    }
}

impl Drop for DebugSessions {
    /// A session ending must not leave a debuggee parked at a breakpoint.
    fn drop(&mut self) {
        for (_, session) in std::mem::take(&mut *self.sessions.lock().unwrap()) {
            drop(session.supervisor.lock().unwrap().take());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wingman_config::PermissionMode;
    use wingman_core::ToolDispatcher;

    fn event(name: &str, body: Value) -> Value {
        json!({ "seq": 0, "type": "event", "event": name, "body": body })
    }

    /// An in-process adapter: one breakpoint hit in `main`, a local `x = 42`,
    /// `x * 2` evaluates to 84, and `continue` runs to exit. It also sends a
    /// reverse request, which the client must refuse rather than ignore.
    fn fake_adapter() -> Arc<DapClient> {
        let (client_io, adapter_io) = tokio::io::duplex(1 << 16);
        let (client_r, client_w) = tokio::io::split(client_io);
        let (adapter_r, mut adapter_w) = tokio::io::split(adapter_io);
        tokio::spawn(async move {
            let mut reader = BufReader::new(adapter_r);
            let mut launch_seq = Value::Null;
            while let Ok(Some(msg)) = read_message(&mut reader).await {
                if msg["type"] == "response" {
                    assert_eq!(msg["command"], "runInTerminal");
                    assert_eq!(msg["success"], false, "reverse request must be refused");
                    continue;
                }
                let command = msg["command"].as_str().unwrap_or("").to_string();
                let args = &msg["arguments"];
                let ok = |body: Value| {
                    json!({ "seq": 0, "type": "response", "request_seq": msg["seq"],
                            "success": true, "command": command, "body": body })
                };
                let mut out = Vec::new();
                match command.as_str() {
                    "initialize" => {
                        assert_eq!(args["linesStartAt1"], true);
                        out.push(ok(json!({ "supportsConfigurationDoneRequest": true })));
                    }
                    "launch" => {
                        assert_eq!(args["program"], "/src/main");
                        launch_seq = msg["seq"].clone();
                        out.push(json!({ "seq": 1, "type": "request",
                                         "command": "runInTerminal", "arguments": {} }));
                        out.push(event("initialized", json!({})));
                    }
                    "setBreakpoints" => {
                        let bps = args["breakpoints"].as_array().unwrap();
                        let verified: Vec<Value> = bps
                            .iter()
                            .map(|b| json!({ "verified": true, "line": b["line"] }))
                            .collect();
                        out.push(ok(json!({ "breakpoints": verified })));
                    }
                    "configurationDone" => {
                        out.push(ok(json!({})));
                        out.push(
                            json!({ "seq": 0, "type": "response", "request_seq": launch_seq,
                                         "success": true, "command": "launch" }),
                        );
                        out.push(event(
                            "output",
                            json!({ "category": "telemetry", "output": "noise" }),
                        ));
                        out.push(event(
                            "output",
                            json!({ "category": "stdout", "output": "hello\n" }),
                        ));
                        out.push(event(
                            "stopped",
                            json!({ "reason": "breakpoint", "threadId": 1 }),
                        ));
                    }
                    "stackTrace" => out.push(ok(json!({
                        "stackFrames": [{ "id": 7, "name": "main", "line": 3,
                                          "source": { "path": "/src/main.rs" } }],
                        "totalFrames": 1
                    }))),
                    "scopes" => {
                        assert_eq!(args["frameId"], 7);
                        out.push(ok(json!({ "scopes": [
                            { "name": "Locals", "variablesReference": 10, "expensive": false },
                            { "name": "Registers", "variablesReference": 11, "expensive": true }
                        ] })));
                    }
                    "variables" => {
                        assert_eq!(args["variablesReference"], 10, "expensive scope fetched");
                        out.push(ok(json!({ "variables": [
                            { "name": "x", "value": "42", "type": "i32", "variablesReference": 0 }
                        ] })));
                    }
                    "evaluate" => {
                        assert_eq!(args["frameId"], 7);
                        out.push(ok(json!({ "result": "84", "type": "i32" })));
                    }
                    "continue" => {
                        assert_eq!(args["threadId"], 1);
                        out.push(ok(json!({})));
                        out.push(event("exited", json!({ "exitCode": 0 })));
                        out.push(event("terminated", json!({})));
                    }
                    "disconnect" => out.push(ok(json!({}))),
                    other => out.push(json!({ "seq": 0, "type": "response",
                        "request_seq": msg["seq"], "success": false, "command": other,
                        "message": format!("unexpected {other}") })),
                }
                for m in out {
                    write_message(&mut adapter_w, &m).await.unwrap();
                }
            }
        });
        DapClient::connect(client_r, client_w)
    }

    #[tokio::test]
    async fn launch_hits_a_breakpoint_then_inspects_evaluates_and_runs_to_exit() {
        let client = fake_adapter();
        let report = client
            .launch(
                "fake",
                json!({ "program": "/src/main", "stopOnEntry": false }),
                &[(
                    PathBuf::from("/src/main.rs"),
                    vec![Breakpoint {
                        line: 3,
                        condition: Some("x > 1".into()),
                    }],
                )],
            )
            .await
            .expect("launch");
        assert!(report.contains("main.rs:3 verified"), "{report}");

        let tmp = std::env::temp_dir();
        let reg =
            crate::ToolRegistry::new(crate::ToolCtx::new(PermissionMode::Yolo, tmp.clone(), tmp))
                .with_builtins();
        let id = reg.ctx().debug.insert(client, None);

        let state = reg
            .dispatch("debug_state", json!({ "session": id, "wait_secs": 5 }))
            .await;
        assert!(!state.is_error, "{}", state.content);
        for want in [
            "stopped: breakpoint (thread 1)",
            "frame 7: main  /src/main.rs:3",
            "Locals:",
            "x = 42 (i32)",
            "hello",
        ] {
            assert!(
                state.content.contains(want),
                "missing {want:?} in:\n{}",
                state.content
            );
        }
        assert!(!state.content.contains("noise"), "telemetry leaked");

        let eval = reg
            .dispatch(
                "debug_eval",
                json!({ "session": id, "expression": "x * 2" }),
            )
            .await;
        assert_eq!(eval.content, "84 (i32)");

        let ran = reg
            .dispatch("debug_continue", json!({ "session": id, "wait_secs": 5 }))
            .await;
        assert!(
            ran.content.contains("program exited with code 0"),
            "{}",
            ran.content
        );

        let step = reg
            .dispatch("debug_continue", json!({ "session": id, "action": "over" }))
            .await;
        assert!(
            step.is_error && step.content.contains("session is over"),
            "{}",
            step.content
        );

        let stopped = reg.dispatch("debug_stop", json!({ "session": id })).await;
        assert!(!stopped.is_error, "{}", stopped.content);
        let gone = reg.dispatch("debug_state", json!({ "session": id })).await;
        assert!(gone.is_error && gone.content.contains("no such debug session"));
    }

    #[tokio::test]
    async fn a_launch_the_adapter_rejects_is_an_error_not_a_hang() {
        let (client_io, adapter_io) = tokio::io::duplex(1 << 12);
        let (client_r, client_w) = tokio::io::split(client_io);
        let (adapter_r, mut adapter_w) = tokio::io::split(adapter_io);
        tokio::spawn(async move {
            let mut reader = BufReader::new(adapter_r);
            while let Ok(Some(msg)) = read_message(&mut reader).await {
                let (success, body) = match msg["command"].as_str() {
                    Some("initialize") => (true, json!({})),
                    _ => (
                        false,
                        json!({ "error": { "format": "no such file: /nope" } }),
                    ),
                };
                let reply = json!({ "seq": 0, "type": "response", "request_seq": msg["seq"],
                                    "success": success, "command": msg["command"], "body": body });
                write_message(&mut adapter_w, &reply).await.unwrap();
            }
        });
        let client = DapClient::connect(client_r, client_w);
        let err = client
            .launch("fake", json!({ "program": "/nope" }), &[])
            .await
            .unwrap_err();
        assert!(err.contains("no such file: /nope"), "{err}");
    }

    #[tokio::test]
    async fn an_adapter_that_dies_wakes_its_waiters() {
        let (client_io, adapter_io) = tokio::io::duplex(1 << 12);
        let (client_r, client_w) = tokio::io::split(client_io);
        drop(adapter_io);
        let client = DapClient::connect(client_r, client_w);
        let err = client
            .request("threads", json!({}), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.contains("exited") || err.contains("closed"), "{err}");
        // The write can fail before the reader sees EOF; state waits for it.
        assert!(client
            .state(Duration::from_secs(5))
            .await
            .contains("exited"));
    }

    /// A debug session runs a program: read-only mode refuses it, and the
    /// debuggee faces the project denylist before any adapter is looked for.
    #[tokio::test]
    async fn debug_start_goes_through_the_shell_gate() {
        let tmp = std::env::temp_dir();
        let read_only = crate::ToolRegistry::new(crate::ToolCtx::new(
            PermissionMode::ReadOnly,
            tmp.clone(),
            tmp.clone(),
        ))
        .with_builtins();
        let refused = read_only
            .dispatch("debug_start", json!({ "program": "app" }))
            .await;
        assert!(refused.is_error, "{}", refused.content);

        let denied = crate::ToolRegistry::new(crate::ToolCtx::new_with_config(
            PermissionMode::Yolo,
            tmp.clone(),
            tmp,
            vec!["forbidden-tool".into()],
            false,
        ))
        .with_builtins();
        let out = denied
            .dispatch(
                "debug_start",
                json!({ "program": "forbidden-tool", "language": "rust" }),
            )
            .await;
        assert!(
            out.is_error && out.content.contains("denylist"),
            "{}",
            out.content
        );
    }

    #[test]
    fn language_is_picked_from_the_program_unless_given() {
        let pick = DebugLang::pick;
        assert_eq!(pick(None, Some("app.py"), false), Ok(DebugLang::Python));
        assert_eq!(pick(None, Some("main.go"), false), Ok(DebugLang::Go));
        assert_eq!(
            pick(None, Some("target/debug/app"), false),
            Ok(DebugLang::Native)
        );
        assert_eq!(pick(None, None, true), Ok(DebugLang::Python));
        assert_eq!(
            pick(Some("Go"), Some("./cmd/server"), false),
            Ok(DebugLang::Go)
        );
        assert!(pick(Some("cobol"), None, false).is_err());
        for lang in DebugLang::ALL {
            assert!(!Adapter::candidates(lang).is_empty());
        }
    }

    #[test]
    fn tcp_adapters_get_their_port_substituted() {
        assert_eq!(
            GO[0].command_line(Some(4711)),
            "dlv dap --listen 127.0.0.1:4711"
        );
        assert_eq!(PYTHON[0].command_line(None), "python3 -m debugpy.adapter");
    }
}
