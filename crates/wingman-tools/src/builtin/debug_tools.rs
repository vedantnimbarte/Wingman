//! `debug_*` — drive a real debugger through the Debug Adapter Protocol.
//!
//! `debug_start` launches a program under lldb-dap / debugpy / Delve and
//! returns a session id; the rest act on that session. See [`crate::dap`].

use crate::dap::{Adapter, Breakpoint, DapClient, DebugLang, Transport};
use crate::{Capability, Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use wingman_core::{ToolOutcome, ToolSpec};

const DEFAULT_WAIT_SECS: u64 = 30;
const MAX_WAIT_SECS: u64 = 120;

fn wait(secs: Option<u64>, default: u64) -> Duration {
    Duration::from_secs(secs.unwrap_or(default).min(MAX_WAIT_SECS))
}

fn parse<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, ToolOutcome> {
    serde_json::from_value(args).map_err(|e| ToolOutcome::err(format!("invalid args: {e}")))
}

fn session(ctx: &ToolCtx, id: &str) -> Result<Arc<DapClient>, ToolOutcome> {
    ctx.debug.get(id).map_err(ToolOutcome::err)
}

fn done(r: Result<String, String>) -> ToolOutcome {
    match r {
        Ok(s) => ToolOutcome::ok(s),
        Err(e) => ToolOutcome::err(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BreakpointArg {
    file: String,
    line: u32,
    #[serde(default)]
    condition: Option<String>,
}

/// Group breakpoints by resolved file, in first-seen order: DAP sets them a
/// whole file at a time.
fn group(
    ctx: &ToolCtx,
    bps: Vec<BreakpointArg>,
) -> Result<Vec<(PathBuf, Vec<Breakpoint>)>, String> {
    let mut files: Vec<(PathBuf, Vec<Breakpoint>)> = Vec::new();
    for b in bps {
        if b.line == 0 {
            return Err(format!(
                "breakpoint lines are 1-based; got 0 for {}",
                b.file
            ));
        }
        let path = ctx.resolve(&b.file);
        let bp = Breakpoint {
            line: b.line,
            condition: b.condition.filter(|c| !c.trim().is_empty()),
        };
        match files.iter_mut().find(|(p, _)| *p == path) {
            Some((_, list)) => list.push(bp),
            None => files.push((path, vec![bp])),
        }
    }
    Ok(files)
}

fn breakpoints_schema() -> Value {
    json!({
        "type": "array",
        "description": "Line breakpoints: [{file, line (1-based), condition?}].",
        "items": {
            "type": "object",
            "properties": {
                "file": { "type": "string" },
                "line": { "type": "integer", "minimum": 1 },
                "condition": { "type": "string", "description": "Only stop when this expression is true." }
            },
            "required": ["file", "line"],
            "additionalProperties": false
        }
    })
}

// ---- debug_start ----------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    #[serde(default)]
    program: Option<String>,
    #[serde(default)]
    module: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    test: bool,
    #[serde(default)]
    breakpoints: Vec<BreakpointArg>,
    #[serde(default)]
    stop_on_entry: Option<bool>,
    #[serde(default)]
    wait_secs: Option<u64>,
}

pub struct DebugStart;

#[async_trait]
impl Tool for DebugStart {
    fn capabilities(&self) -> Capability {
        // It runs a program: the same grant as run_shell.
        Capability::SHELL
    }

    /// Launch can spend a minute loading symbols and then wait for the first
    /// stop; every step of it is individually bounded.
    fn owns_timeout(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "debug_start".into(),
            description:
                "Launch a program under a real debugger (lldb-dap for Rust/C/C++, debugpy \
                          for Python, Delve for Go) and return a session id. Pass `breakpoints` \
                          here so they are set before the program runs; with none it stops on \
                          entry. Waits for the first stop and reports the stack and locals. Then \
                          use debug_state, debug_continue, debug_eval, debug_breakpoints, and \
                          debug_stop. For a Rust test, point `program` at the test binary cargo \
                          built (target/debug/deps/<crate>-<hash>)."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "program": { "type": "string", "description": "Binary, Python script, or Go package/file to debug." },
                    "module": { "type": "string", "description": "Python only: run `python -m <module>` instead of a script, e.g. `pytest`." },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Program arguments." },
                    "cwd": { "type": "string", "description": "Working directory; defaults to project root." },
                    "language": { "type": "string", "enum": ["rust", "c", "cpp", "python", "go"], "description": "Which debugger; inferred from `program` when omitted (.py → python, .go → go, else native)." },
                    "test": { "type": "boolean", "default": false, "description": "Go only: debug the package's tests." },
                    "breakpoints": breakpoints_schema(),
                    "stop_on_entry": { "type": "boolean", "description": "Default: true when no breakpoints are given." },
                    "wait_secs": { "type": "integer", "minimum": 0, "maximum": MAX_WAIT_SECS, "description": "How long to wait for the first stop (default 30)." }
                },
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let a: StartArgs = match parse(args) {
            Ok(a) => a,
            Err(o) => return o,
        };
        if a.program.is_none() == a.module.is_none() {
            return ToolOutcome::err(
                "give exactly one of `program` (binary, script, or Go package) or `module` (Python -m)",
            );
        }
        let lang = match DebugLang::pick(
            a.language.as_deref(),
            a.program.as_deref(),
            a.module.is_some(),
        ) {
            Ok(l) => l,
            Err(e) => return ToolOutcome::err(e),
        };
        let cwd = a
            .cwd
            .as_deref()
            .map(|c| ctx.resolve(c))
            .unwrap_or_else(|| ctx.project_root.clone());
        let program = a.program.as_deref().map(|p| ctx.resolve(p));

        // The debuggee is what actually runs, so it faces the denylist a shell
        // command would. The adapter command faces it again in `prepare`.
        // As given, not resolved: the denylist tokenizer reads `\` as an
        // escape, so a resolved Windows path would hide the program's name.
        let debuggee = match (&a.program, &a.module) {
            (Some(p), _) => p.clone(),
            (None, Some(m)) => format!("python -m {m}"),
            (None, None) => unreachable!("checked above"),
        };
        let command_line = std::iter::once(debuggee)
            .chain(a.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");
        if ctx.is_shell_denied(&command_line) {
            return ToolOutcome::err(format!(
                "debug target denied by project denylist: {command_line}"
            ));
        }
        let bps = match group(ctx, a.breakpoints) {
            Ok(b) => b,
            Err(e) => return ToolOutcome::err(e),
        };

        let detected = tokio::task::spawn_blocking(move || Adapter::detect(lang))
            .await
            .ok()
            .flatten();
        let Some(adapter) = detected else {
            return ToolOutcome::ok(format!(
                "(no {} debug adapter on PATH. Install {}, then retry. Until then, fall back \
                 to run_shell with logging or a focused test.)",
                lang.label(),
                lang.install_hint()
            ));
        };

        let (client, supervisor) = match spawn_adapter(&adapter, &cwd, ctx).await {
            Ok(v) => v,
            Err(e) => return ToolOutcome::err(e),
        };
        let mut launch = json!({
            "args": a.args,
            "cwd": cwd,
            "stopOnEntry": a.stop_on_entry.unwrap_or(bps.is_empty()),
        });
        if let Some(p) = &program {
            launch["program"] = json!(p);
        }
        match lang {
            DebugLang::Python => {
                launch["console"] = json!("internalConsole");
                if let Some(m) = &a.module {
                    launch["module"] = json!(m);
                }
            }
            DebugLang::Go => launch["mode"] = json!(if a.test { "test" } else { "debug" }),
            DebugLang::Native => {}
        }
        let report = match client.launch(adapter.program, launch, &bps).await {
            Ok(r) => r,
            // `supervisor` drops here, taking the adapter and anything it started.
            Err(e) => {
                return ToolOutcome::err(format!("{} could not launch: {e}", adapter.program))
            }
        };
        let id = ctx.debug.insert(client.clone(), Some(supervisor));
        let state = client.state(wait(a.wait_secs, DEFAULT_WAIT_SECS)).await;
        let report = if report.is_empty() {
            String::new()
        } else {
            format!("{report}\n")
        };
        ToolOutcome::ok(format!(
            "started {id} under {}. It is killed with its process tree on debug_stop or when \
             the session ends.\n{report}{state}",
            adapter.program
        ))
    }
}

/// Spawn `adapter` through exactly `run_shell`'s preparation — mode,
/// denylist, sandbox, credential scrub — under a process-tree supervisor,
/// and connect to it.
async fn spawn_adapter(
    adapter: &Adapter,
    cwd: &std::path::Path,
    ctx: &ToolCtx,
) -> Result<(Arc<DapClient>, crate::child_process::Supervisor), String> {
    let port = match adapter.transport {
        Transport::Stdio => None,
        // ponytail: bind-then-release can lose the port to another process
        // before the adapter binds it; the connect below then fails loudly.
        Transport::Tcp => Some(
            std::net::TcpListener::bind("127.0.0.1:0")
                .and_then(|l| l.local_addr())
                .map(|a| a.port())
                .map_err(|e| format!("no free local port for {}: {e}", adapter.program))?,
        ),
    };
    let cmd = super::run_shell::prepare_command(
        &adapter.command_line(port),
        Some(cwd.display().to_string()),
        ctx,
    )?;
    let mut supervised = crate::child_process::SupervisedCommand::from_command(cmd);
    let piped = || {
        if port.is_none() {
            Stdio::piped()
        } else {
            Stdio::null()
        }
    };
    supervised
        .command_mut()
        .stdin(piped())
        .stdout(piped())
        .stderr(Stdio::null());
    let mut supervisor = supervised
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", adapter.program))?;
    let child = supervisor
        .child_mut()
        .ok_or("the debug adapter vanished after spawn")?;
    let client = match port {
        None => {
            let (Some(out), Some(input)) = (child.stdout.take(), child.stdin.take()) else {
                return Err("the debug adapter has no stdio".into());
            };
            DapClient::connect(out, input)
        }
        Some(port) => {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            loop {
                match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                    Ok(stream) => {
                        let (r, w) = stream.into_split();
                        break DapClient::connect(r, w);
                    }
                    Err(_) if tokio::time::Instant::now() < deadline => {
                        if let Ok(Some(status)) = child.try_wait() {
                            return Err(format!(
                                "{} exited before accepting a connection ({status})",
                                adapter.program
                            ));
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(e) => {
                        return Err(format!(
                            "could not connect to {} on port {port}: {e}",
                            adapter.program
                        ))
                    }
                }
            }
        }
    };
    Ok((client, supervisor))
}

// ---- debug_breakpoints ----------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BreakpointsArgs {
    session: String,
    #[serde(default)]
    breakpoints: Vec<BreakpointArg>,
    #[serde(default)]
    clear: Vec<String>,
}

pub struct DebugBreakpoints;

#[async_trait]
impl Tool for DebugBreakpoints {
    fn capabilities(&self) -> Capability {
        Capability::SHELL
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "debug_breakpoints".into(),
            description: "Set line breakpoints in a debug session. For each file named, the given \
                          breakpoints REPLACE that file's existing ones; files not named are \
                          untouched. `clear` removes all breakpoints from the listed files."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session id from debug_start, e.g. `dbg-1`." },
                    "breakpoints": breakpoints_schema(),
                    "clear": { "type": "array", "items": { "type": "string" }, "description": "Files to clear." }
                },
                "required": ["session"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let a: BreakpointsArgs = match parse(args) {
            Ok(a) => a,
            Err(o) => return o,
        };
        let client = match session(ctx, &a.session) {
            Ok(c) => c,
            Err(o) => return o,
        };
        let mut files = match group(ctx, a.breakpoints) {
            Ok(f) => f,
            Err(e) => return ToolOutcome::err(e),
        };
        files.extend(a.clear.iter().map(|f| (ctx.resolve(f), Vec::new())));
        if files.is_empty() {
            return ToolOutcome::err("nothing to do: give `breakpoints` or `clear`");
        }
        let mut report = Vec::new();
        for (path, bps) in &files {
            match client.set_breakpoints(path, bps).await {
                Ok(r) => report.push(r),
                Err(e) => return ToolOutcome::err(e),
            }
        }
        ToolOutcome::ok(report.join("\n"))
    }
}

// ---- debug_continue -------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContinueArgs {
    session: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    wait_secs: Option<u64>,
}

pub struct DebugContinue;

#[async_trait]
impl Tool for DebugContinue {
    fn capabilities(&self) -> Capability {
        Capability::SHELL
    }

    fn owns_timeout(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "debug_continue".into(),
            description: "Resume a stopped debug session: `continue` to the next breakpoint, or \
                          step `over`, `in`, or `out`. Waits for the next stop (or exit) and \
                          reports the stack and locals, like debug_state."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string" },
                    "action": { "type": "string", "enum": ["continue", "over", "in", "out"], "default": "continue" },
                    "wait_secs": { "type": "integer", "minimum": 0, "maximum": MAX_WAIT_SECS, "description": "How long to wait for the next stop (default 30)." }
                },
                "required": ["session"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let a: ContinueArgs = match parse(args) {
            Ok(a) => a,
            Err(o) => return o,
        };
        let client = match session(ctx, &a.session) {
            Ok(c) => c,
            Err(o) => return o,
        };
        let action = a.action.as_deref().unwrap_or("continue");
        done(
            client
                .resume(action, wait(a.wait_secs, DEFAULT_WAIT_SECS))
                .await,
        )
    }
}

// ---- debug_state ----------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateArgs {
    session: String,
    #[serde(default)]
    wait_secs: Option<u64>,
}

pub struct DebugState;

#[async_trait]
impl Tool for DebugState {
    fn capabilities(&self) -> Capability {
        // Not READ: rendering locals has the adapter format values, which in
        // Python means calling the program's own `__repr__`.
        Capability::SHELL
    }

    fn owns_timeout(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "debug_state".into(),
            description: "Where a debug session is now: why it stopped, the stack (up to 20 \
                          frames, with frame ids for debug_eval), the top frame's locals, and \
                          program output since the last check. If it is running, waits up to \
                          `wait_secs` (default 0) for it to stop."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string" },
                    "wait_secs": { "type": "integer", "minimum": 0, "maximum": MAX_WAIT_SECS }
                },
                "required": ["session"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let a: StateArgs = match parse(args) {
            Ok(a) => a,
            Err(o) => return o,
        };
        match session(ctx, &a.session) {
            Ok(client) => ToolOutcome::ok(client.state(wait(a.wait_secs, 0)).await),
            Err(o) => o,
        }
    }
}

// ---- debug_eval -----------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalArgs {
    session: String,
    expression: String,
    #[serde(default)]
    frame_id: Option<i64>,
}

pub struct DebugEval;

#[async_trait]
impl Tool for DebugEval {
    fn capabilities(&self) -> Capability {
        // An expression can call any function in the program.
        Capability::SHELL
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "debug_eval".into(),
            description: "Evaluate an expression in a stopped debug session, in the top frame or \
                          the `frame_id` debug_state listed. The expression can call functions \
                          and change program state."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string" },
                    "expression": { "type": "string" },
                    "frame_id": { "type": "integer", "description": "A frame id from debug_state; default the top frame." }
                },
                "required": ["session", "expression"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let a: EvalArgs = match parse(args) {
            Ok(a) => a,
            Err(o) => return o,
        };
        match session(ctx, &a.session) {
            Ok(client) => done(client.evaluate(&a.expression, a.frame_id).await),
            Err(o) => o,
        }
    }
}

// ---- debug_stop -----------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StopArgs {
    session: String,
}

pub struct DebugStop;

#[async_trait]
impl Tool for DebugStop {
    fn capabilities(&self) -> Capability {
        Capability::SHELL
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "debug_stop".into(),
            description: "End a debug session: terminate the program and kill the debugger with \
                          its whole process tree."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "session": { "type": "string" } },
                "required": ["session"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let a: StopArgs = match parse(args) {
            Ok(a) => a,
            Err(o) => return o,
        };
        match ctx.debug.stop(&a.session).await {
            Ok(()) => ToolOutcome::ok(format!("stopped {} and killed its process tree", a.session)),
            Err(e) => ToolOutcome::err(e),
        }
    }
}
