//! E3 — executable acceptance checks.
//!
//! Workers attach a list of [`crate::model::Acceptance`] checks to their
//! task; before reporting Review, they call the `run_acceptance` tool,
//! which runs every check via [`run_acceptance_checks`] and surfaces the
//! results back to the model. The worker must include the results in
//! `task_complete`; the orchestrator gates the Review transition on every
//! check being green.
//!
//! ## Why this matters
//!
//! Without acceptance, the only signal that a worker "finished" is the
//! model's word. Models hallucinate. Executable acceptance — concrete
//! `cargo check`, `cargo test`, `grep` for an expected string — turns a
//! self-report into a verifiable claim that the orchestrator can
//! independently validate.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::model::Acceptance;

/// Result of running one [`Acceptance`] check.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceptanceResult {
    /// Stable label describing which check ran. Includes the kind +
    /// enough payload for the model + the parent log to identify it.
    pub label: String,
    /// Did the check succeed?
    pub ok: bool,
    /// Best-effort tail of stdout/stderr or the matched text. Capped to
    /// keep token usage bounded.
    pub output: String,
    /// J15 — passing tests the command reported, when its output carried a
    /// test-runner summary [`passed_tests`] recognises. Parsed from the whole
    /// output rather than the tail: a workspace `cargo test` prints one
    /// summary per test binary, and the tail holds only the last few.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed_tests: Option<u32>,
}

impl AcceptanceResult {
    pub fn ok(label: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ok: true,
            output: output.into(),
            passed_tests: None,
        }
    }
    pub fn fail(label: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ok: false,
            output: output.into(),
            passed_tests: None,
        }
    }
}

/// Are all results green?
pub fn all_green(results: &[AcceptanceResult]) -> bool {
    !results.is_empty() && results.iter().all(|r| r.ok)
}

/// Run every acceptance check sequentially.
///
/// `cwd` is the worker's worktree — shell commands inherit it, grep
/// paths resolve against it.
///
/// Each check has a hard per-check timeout (default 60s). The whole
/// function is synchronous; callers in async contexts should wrap with
/// `tokio::task::spawn_blocking`.
pub fn run_acceptance_checks(checks: &[Acceptance], cwd: &Path) -> Vec<AcceptanceResult> {
    run_acceptance_checks_within(checks, cwd, DEFAULT_SHELL_TIMEOUT)
}

/// Run every check, sharing one `budget` across the whole set.
///
/// The budget is a deadline, not a per-check allowance: checks run in sequence
/// and each gets what is left. A single slow check therefore cannot multiply
/// into `n * budget`, and a caller that passes the task's own timeout gets
/// acceptance bounded by the same number that bounds everything else — rather
/// than by a constant that has no idea how long this project takes to build.
pub fn run_acceptance_checks_within(
    checks: &[Acceptance],
    cwd: &Path,
    budget: Duration,
) -> Vec<AcceptanceResult> {
    let deadline = std::time::Instant::now() + budget;
    let mut out = Vec::with_capacity(checks.len());
    for c in checks {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        out.push(run_one(c, cwd, left));
    }
    out
}

fn run_one(check: &Acceptance, cwd: &Path, timeout: Duration) -> AcceptanceResult {
    match check {
        Acceptance::Shell { cmd } => run_shell(cmd, cwd, timeout),
        Acceptance::Grep { pattern, path } => run_grep(pattern, path, cwd),
        // J6 — real HTTP GET via `curl` (no async runtime, no new dep). The
        // status line proves reachability; `must_match` asserts on body/code,
        // `schema` on the shape of the JSON body.
        Acceptance::Http {
            url,
            must_match,
            schema,
        } => run_http(url, must_match, schema.as_ref(), cwd),
        // J6 — run the app: execute the script (or the target as a
        // command) like a shell check, but label it as a run.
        Acceptance::Run { target, script } => {
            let cmd = script.clone().unwrap_or_else(|| target.clone());
            let mut res = run_shell(&cmd, cwd, timeout);
            res.label = format!("run: {target}");
            res
        }
        // J6 — assert a rendered artifact contains expected text.
        Acceptance::Assert {
            screenshot,
            must_contain_text,
        } => run_assert(screenshot, must_contain_text, cwd),
    }
}

/// J6 — real HTTP GET, shelling to `curl` so the sync runner stays
/// dependency-free (no reqwest, no tokio). `curl` prints the body followed
/// by a final `\n<status>` line (via `-w`); we split that off and assert:
///
/// - `must_match` is a **number** → the HTTP status must equal it.
/// - `must_match` is a **string** → the body must contain it (and status
///   must be < 400).
/// - `must_match` is **null/absent** → status must be < 400.
/// - anything else (object/array) → its compact JSON form must appear in the
///   body (and status < 400) — a coarse "shape present" check.
///
/// `schema`, when given, is checked on top of that: the body must parse as
/// JSON and validate against it (see [`schema_errors`]).
fn run_http(
    url: &str,
    must_match: &serde_json::Value,
    schema: Option<&serde_json::Value>,
    cwd: &Path,
) -> AcceptanceResult {
    let label = format!("http: {url}");
    // -sS quiet-but-show-errors, -L follow redirects, -m 30 hard timeout,
    // -w appends the numeric status on its own trailing line.
    let output = Command::new("curl")
        .args(["-sSL", "-m", "30", "-o", "-", "-w", "\n%{http_code}", url])
        .current_dir(cwd)
        .output();
    let output = match output {
        Ok(o) => o,
        Err(e) => return AcceptanceResult::fail(label, format!("curl spawn failed: {e}")),
    };
    if !output.status.success() {
        return AcceptanceResult::fail(
            label,
            format!(
                "curl exited non-zero: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        );
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let (body, status_str) = match raw.rsplit_once('\n') {
        Some((b, s)) => (b, s.trim()),
        None => ("", raw.trim()),
    };
    let status: u32 = status_str.parse().unwrap_or(0);
    let res = assert_http(label, status, body, must_match);
    match schema {
        Some(schema) if res.ok => assert_schema(res, body, schema),
        _ => res,
    }
}

/// Validate a JSON `body` against `schema`, turning a passing [`assert_http`]
/// result into a failure when it does not.
fn assert_schema(
    res: AcceptanceResult,
    body: &str,
    schema: &serde_json::Value,
) -> AcceptanceResult {
    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            return AcceptanceResult::fail(
                res.label,
                format!("{}, body is not JSON: {e}", res.output),
            )
        }
    };
    let mut errs = Vec::new();
    schema_errors(&value, schema, "$", &mut errs);
    if errs.is_empty() {
        AcceptanceResult::ok(res.label, format!("{}, schema matched", res.output))
    } else {
        let shown = errs.iter().take(5).cloned().collect::<Vec<_>>().join("; ");
        AcceptanceResult::fail(
            res.label,
            format!(
                "{}, schema mismatch ({} error(s)): {shown}",
                res.output,
                errs.len()
            ),
        )
    }
}

/// JSON Schema keywords [`schema_errors`] treats as annotations and ignores.
/// `format` is annotation-only by default in draft 2020-12, too.
const SCHEMA_ANNOTATIONS: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "title",
    "description",
    "default",
    "examples",
    "format",
];

/// Validate `value` against a JSON Schema, appending one message per
/// violation (prefixed with its JSON path) to `errs`.
///
/// Covers the keywords an acceptance check realistically asserts on: `type`,
/// `enum`, `const`, `properties`, `required`, `additionalProperties`, `items`,
/// `min/maxItems`, `min/maxLength`, `pattern`, `minimum`/`maximum` (and the
/// exclusive forms), `allOf`/`anyOf`/`oneOf`/`not`.
///
/// ponytail: a subset, not a full validator (no `$ref`, `patternProperties`,
/// `if/then/else`, …). Any keyword outside the subset is reported as an error
/// rather than skipped, so a schema this cannot fully check fails the
/// acceptance check instead of passing it unexamined. Swap in the
/// `jsonschema` crate if planners start writing schemas that need more.
fn schema_errors(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    path: &str,
    errs: &mut Vec<String>,
) {
    use serde_json::Value;
    let obj = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            return errs.push(format!("{path}: schema `false` rejects every value"))
        }
        Value::Object(o) => o,
        other => {
            return errs.push(format!(
                "{path}: schema must be an object or boolean, got {other}"
            ))
        }
    };
    let num = |k: &str| obj.get(k).and_then(Value::as_f64);
    let len = |k: &str| obj.get(k).and_then(Value::as_u64);
    for (key, kw) in obj {
        match key.as_str() {
            "type" => {
                let names: Vec<&str> = match kw {
                    Value::String(s) => vec![s.as_str()],
                    Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
                    _ => vec![],
                };
                if !names.iter().any(|n| json_type_matches(value, n)) {
                    errs.push(format!(
                        "{path}: expected type {}, got {}",
                        names.join("|"),
                        json_type_name(value)
                    ));
                }
            }
            "enum" => {
                if !kw.as_array().is_some_and(|a| a.contains(value)) {
                    errs.push(format!("{path}: {value} is not one of {kw}"));
                }
            }
            "const" => {
                if kw != value {
                    errs.push(format!("{path}: expected {kw}, got {value}"));
                }
            }
            "properties" => {
                if let (Some(props), Value::Object(v)) = (kw.as_object(), value) {
                    for (name, sub) in props {
                        if let Some(child) = v.get(name) {
                            schema_errors(child, sub, &format!("{path}.{name}"), errs);
                        }
                    }
                }
            }
            "required" => {
                if let Value::Object(v) = value {
                    for name in kw
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        if !v.contains_key(name) {
                            errs.push(format!("{path}: missing required property `{name}`"));
                        }
                    }
                }
            }
            "additionalProperties" => {
                if let Value::Object(v) = value {
                    let declared = obj.get("properties").and_then(Value::as_object);
                    for (name, child) in v {
                        if !declared.is_some_and(|d| d.contains_key(name)) {
                            schema_errors(child, kw, &format!("{path}.{name}"), errs);
                        }
                    }
                }
            }
            "items" => {
                if let Value::Array(items) = value {
                    for (i, child) in items.iter().enumerate() {
                        schema_errors(child, kw, &format!("{path}[{i}]"), errs);
                    }
                }
            }
            "minItems" | "maxItems" => {
                if let (Value::Array(a), Some(n)) = (value, len(key)) {
                    if (key == "minItems" && (a.len() as u64) < n)
                        || (key == "maxItems" && a.len() as u64 > n)
                    {
                        errs.push(format!("{path}: {} item(s) violates {key} {n}", a.len()));
                    }
                }
            }
            "minLength" | "maxLength" => {
                if let (Value::String(s), Some(n)) = (value, len(key)) {
                    let chars = s.chars().count() as u64;
                    if (key == "minLength" && chars < n) || (key == "maxLength" && chars > n) {
                        errs.push(format!("{path}: length {chars} violates {key} {n}"));
                    }
                }
            }
            "pattern" => {
                if let (Value::String(s), Some(p)) = (value, kw.as_str()) {
                    match regex::Regex::new(p) {
                        Ok(re) if re.is_match(s) => {}
                        Ok(_) => errs.push(format!("{path}: {s:?} does not match pattern {p:?}")),
                        Err(e) => errs.push(format!("{path}: invalid pattern {p:?}: {e}")),
                    }
                }
            }
            "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" => {
                if let (Some(v), Some(n)) = (value.as_f64(), num(key)) {
                    let ok = match key.as_str() {
                        "minimum" => v >= n,
                        "maximum" => v <= n,
                        "exclusiveMinimum" => v > n,
                        _ => v < n,
                    };
                    if !ok {
                        errs.push(format!("{path}: {v} violates {key} {n}"));
                    }
                }
            }
            "allOf" | "anyOf" | "oneOf" => {
                let subs = kw.as_array().map(Vec::as_slice).unwrap_or_default();
                let results: Vec<Vec<String>> = subs
                    .iter()
                    .map(|s| {
                        let mut e = Vec::new();
                        schema_errors(value, s, path, &mut e);
                        e
                    })
                    .collect();
                let passing = results.iter().filter(|e| e.is_empty()).count();
                match key.as_str() {
                    "allOf" => errs.extend(results.into_iter().flatten()),
                    "anyOf" if passing == 0 => errs.push(format!("{path}: matches none of anyOf")),
                    "oneOf" if passing != 1 => errs.push(format!(
                        "{path}: matches {passing} of oneOf, wanted exactly 1"
                    )),
                    _ => {}
                }
            }
            "not" => {
                let mut e = Vec::new();
                schema_errors(value, kw, path, &mut e);
                if e.is_empty() {
                    errs.push(format!("{path}: matches a `not` schema"));
                }
            }
            k if SCHEMA_ANNOTATIONS.contains(&k) => {}
            k => errs.push(format!("{path}: unsupported schema keyword `{k}`")),
        }
    }
}

fn json_type_matches(value: &serde_json::Value, name: &str) -> bool {
    use serde_json::Value;
    match (name, value) {
        ("integer", Value::Number(n)) => {
            n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        ("number", Value::Number(_)) => true,
        (other, v) => other == json_type_name(v),
    }
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    use serde_json::Value;
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Pure assertion half of [`run_http`] — separated so the match/status
/// logic is unit-testable without a live network.
fn assert_http(
    label: String,
    status: u32,
    body: &str,
    must_match: &serde_json::Value,
) -> AcceptanceResult {
    match must_match {
        serde_json::Value::Number(n) => {
            let want = n.as_u64().unwrap_or(0) as u32;
            if status == want {
                AcceptanceResult::ok(label, format!("status {status}"))
            } else {
                AcceptanceResult::fail(label, format!("status {status}, wanted {want}"))
            }
        }
        serde_json::Value::Null => {
            if (200..400).contains(&status) {
                AcceptanceResult::ok(label, format!("status {status}"))
            } else {
                AcceptanceResult::fail(label, format!("status {status} (not 2xx/3xx)"))
            }
        }
        other => {
            let needle = match other {
                serde_json::Value::String(s) => s.clone(),
                v => v.to_string(),
            };
            if !(200..400).contains(&status) {
                return AcceptanceResult::fail(label, format!("status {status} (not 2xx/3xx)"));
            }
            if body.contains(&needle) {
                AcceptanceResult::ok(label, format!("status {status}, body matched"))
            } else {
                AcceptanceResult::fail(label, format!("status {status}, body missing {needle:?}"))
            }
        }
    }
}

/// J6 — verify a rendered artifact (screenshot / SVG dump) exists and
/// contains every expected text fragment.
///
/// Screenshot *capture* is intentionally not embedded here: an
/// [`Acceptance::Run`] step renders the artifact first (e.g.
/// `chromium --headless --dump-dom <url> > page.html`, or a ratatui SVG
/// dump), and this `Assert` checks it. That composition needs no browser
/// crate and keeps the runner synchronous and dependency-free.
fn run_assert(path: &str, must_contain: &[String], cwd: &Path) -> AcceptanceResult {
    let label = format!("assert: {path}");
    let full = cwd.join(path);
    let body = match std::fs::read_to_string(&full) {
        Ok(b) => b,
        Err(e) => {
            return AcceptanceResult::fail(label, format!("read {} failed: {e}", full.display()))
        }
    };
    let missing: Vec<&str> = must_contain
        .iter()
        .filter(|needle| !body.contains(needle.as_str()))
        .map(|s| s.as_str())
        .collect();
    if missing.is_empty() {
        AcceptanceResult::ok(
            label,
            format!("all {} fragment(s) present", must_contain.len()),
        )
    } else {
        AcceptanceResult::fail(label, format!("missing text: {}", missing.join(", ")))
    }
}

/// Fallback per-check timeout, used only by [`run_acceptance_checks`].
///
/// 60s was the original value and it is wrong for the checks planners
/// actually write. `cargo check` in a freshly created worktree compiles the
/// dependency graph from nothing, which is the *first* run in every worktree,
/// not an edge case — and blowing the cap records a red check indistinguishable
/// from a compile error. Callers that know the task's real budget should pass
/// it via [`run_acceptance_checks_within`]; this is the floor for those that
/// do not.
pub const DEFAULT_SHELL_TIMEOUT: Duration = Duration::from_secs(600);
const OUTPUT_TAIL_BYTES: usize = 1024;

fn run_shell(cmd: &str, cwd: &Path, timeout: Duration) -> AcceptanceResult {
    let label = format!("shell: {cmd}");
    let (program, args) = if cfg!(windows) {
        ("cmd", vec!["/C".to_string(), cmd.to_string()])
    } else {
        ("sh", vec!["-c".to_string(), cmd.to_string()])
    };

    // Stable-Rust has no built-in process timeout. We use a thread +
    // channel pattern (`wait_with_output` doesn't honor a deadline) so
    // hung commands eventually surface as failures instead of pinning a
    // worker forever.
    let started = std::time::Instant::now();
    let child = Command::new(program)
        .args(&args)
        .current_dir(cwd)
        // Never the worker's stdin: that is the manager's IPC pipe, with a
        // thread blocked reading it. On Windows an MSYS tool (`test`, `[`)
        // that inherits a pipe handle with a read pending hangs at startup,
        // so the check never finishes.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return AcceptanceResult::fail(label, format!("spawn failed: {e}")),
    };

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child.wait_with_output().ok();
                let combined = output
                    .map(|o| {
                        let mut s = String::new();
                        if !o.stdout.is_empty() {
                            s.push_str(&String::from_utf8_lossy(&o.stdout));
                        }
                        if !o.stderr.is_empty() {
                            if !s.is_empty() {
                                s.push('\n');
                            }
                            s.push_str(&String::from_utf8_lossy(&o.stderr));
                        }
                        s
                    })
                    .unwrap_or_default();
                let tail = tail_string(&combined, OUTPUT_TAIL_BYTES);
                let mut res = if status.success() {
                    AcceptanceResult::ok(label, tail)
                } else {
                    let code = status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".to_string());
                    AcceptanceResult::fail(label, format!("exit {code}\n{tail}"))
                };
                // Counted whether or not the command passed: a red test run
                // still says how many tests passed, and that is the number the
                // net-negative check compares.
                res.passed_tests = passed_tests(&combined);
                return res;
            }
            Ok(None) => {
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return AcceptanceResult::fail(label, format!("timed out after {timeout:?}"));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                return AcceptanceResult::fail(label, format!("wait failed: {e}"));
            }
        }
    }
}

fn run_grep(pattern: &str, path: &str, cwd: &Path) -> AcceptanceResult {
    let label = format!("grep: `{pattern}` in {path}");
    let full = cwd.join(path);
    let body = match std::fs::read_to_string(&full) {
        Ok(b) => b,
        Err(e) => {
            return AcceptanceResult::fail(label, format!("read {} failed: {e}", full.display()))
        }
    };
    // A check called `grep` gets grep semantics.
    //
    // This was a plain substring match, on the reasoning that planners use it
    // as a cheap "did the string land in the file?" probe. They do not. A live
    // run (#34) had the planner emit
    //
    //     ^///.*default_max_auto_dispatch_per_cycle|fn default_…
    //
    // — anchors, `.*`, alternation — which `str::find` looked for verbatim and
    // never found. The check could not pass at any point, the task failed, and
    // the retry ladder spent two more attempts proving the same impossibility.
    // An acceptance check nothing can satisfy is worse than no check.
    //
    // Literal first: it is cheaper and it is what every plan written against
    // the old behaviour meant, so nothing that passed before can start
    // failing. Then regex, in multi-line mode, because `^` in a grep pattern
    // means "start of line" and matching only the start of the file would
    // honour the syntax while ignoring the intent.
    let found = body.find(pattern).or_else(|| {
        regex::RegexBuilder::new(pattern)
            .multi_line(true)
            .build()
            .ok()
            .and_then(|re| re.find(&body).map(|m| m.start()))
    });

    if let Some(idx) = found {
        // Surface the matching line so the model knows where it hit.
        let line_start = body[..idx].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = body[idx..]
            .find('\n')
            .map(|i| idx + i)
            .unwrap_or(body.len());
        let line = &body[line_start..line_end];
        AcceptanceResult::ok(label, line.to_string())
    } else {
        // Say whether the pattern was even usable as a regex. A planner that
        // wrote a broken one otherwise gets "not found", goes looking for the
        // text, finds it there, and has no idea why the check disagrees.
        let note = match regex::Regex::new(pattern) {
            Ok(_) => String::new(),
            Err(_) => " (not valid regex either; matched literally)".to_string(),
        };
        AcceptanceResult::fail(
            label,
            format!("pattern {pattern:?} not found in {path}{note}"),
        )
    }
}

fn tail_string(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let start = s.len() - max_bytes;
    // Walk forward to the next char boundary so we don't slice mid-UTF8.
    let mut cut = start;
    while !s.is_char_boundary(cut) {
        cut += 1;
        if cut >= s.len() {
            return String::new();
        }
    }
    format!("…{}", &s[cut..])
}

/// Compact summary line for surfacing N results back through stdout / a
/// tool result. Useful for embedding in `task_complete` outputs.
pub fn summarize(results: &[AcceptanceResult]) -> String {
    let total = results.len();
    let failed = results.iter().filter(|r| !r.ok).count();
    if total == 0 {
        return "no acceptance checks defined".into();
    }
    if failed == 0 {
        format!("{total}/{total} green")
    } else {
        // The label alone says *which* check failed and nothing about why,
        // so a check that timed out and a check whose command genuinely
        // returned non-zero produce identical text. That is how a 60s cap on
        // a cold `cargo check` was recorded as if the code did not compile.
        // One line of the detail is enough to tell those apart.
        format!(
            "{}/{total} green; failing: {}",
            total - failed,
            results
                .iter()
                .filter(|r| !r.ok)
                .map(|r| {
                    match r.output.lines().find(|l| !l.trim().is_empty()) {
                        Some(first) => format!("{} ({})", r.label, first.trim()),
                        None => r.label.clone(),
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// J15 — passing tests reported by a test runner's summary, or `None` when
/// `output` carries no summary this recognises. Tried in order, first match
/// wins:
///
/// - `cargo test`: every `test result: … N passed;` line, summed (one per test
///   binary); `cargo nextest`: the `Summary … N passed` line.
/// - jest / vitest: the `Tests: … N passed` line.
/// - pytest: the closing `N passed … in 1.23s` line.
/// - `go test -v`: one `--- PASS:` line per test, subtests included. Plain
///   `go test` prints no per-test lines and so reports nothing.
pub fn passed_tests(output: &str) -> Option<u32> {
    use std::sync::OnceLock;
    static RES: OnceLock<[regex::Regex; 6]> = OnceLock::new();
    let [ansi, cargo, nextest, jest, pytest, go] = RES.get_or_init(|| {
        [
            r"\x1b\[[0-9;]*m",
            r"test result: \w+\. (\d+) passed;",
            r"^\s*Summary \[.*?(\d+) passed",
            r"^\s*Tests:?\s+(?:.*?, )?(\d+) passed",
            r"^=*\s*(?:.*?, )?(\d+) passed.* in [\d.]+s",
            r"^\s*--- PASS: ",
        ]
        .map(|p| regex::Regex::new(&format!("(?m){p}")).expect("static pattern"))
    });
    // Runners colour their summaries when they think they own a terminal.
    let plain = ansi.replace_all(output, "");
    let sum = |re: &regex::Regex| -> Option<u32> {
        let mut found = None;
        for c in re.captures_iter(&plain) {
            let n: u32 = c[1].parse().ok()?;
            found = Some(found.unwrap_or(0) + n);
        }
        found
    };
    let go_passes = go.find_iter(&plain).count() as u32;
    sum(cargo)
        .or_else(|| sum(nextest))
        .or_else(|| sum(jest))
        .or_else(|| sum(pytest))
        .or((go_passes > 0).then_some(go_passes))
}

/// J15 — the label a test-running check's result will carry, or `None` for a
/// check that does not look like it runs tests. Only `shell` and `run` checks
/// execute a command; of those, one whose command mentions `test` (or `jest`)
/// is treated as a test run.
///
/// ponytail: a keyword heuristic — a test runner invoked through a script
/// named without "test" is missed, and its count stays unchecked. A plan field
/// marking a check as the test suite would make it exact.
pub fn test_check_label(check: &Acceptance) -> Option<String> {
    let (label, cmd) = match check {
        Acceptance::Shell { cmd } => (format!("shell: {cmd}"), cmd.as_str()),
        Acceptance::Run { target, script } => (
            format!("run: {target}"),
            script.as_deref().unwrap_or(target),
        ),
        _ => return None,
    };
    let cmd = cmd.to_ascii_lowercase();
    (cmd.contains("test") || cmd.contains("jest")).then_some(label)
}

/// J15 — compare an attempt's test counts with the base-commit counts for the
/// same checks. Returns `(before, after)` summed over the labels both sides
/// counted, or `None` when they share none — a check whose output carried no
/// summary on one side says nothing about the other.
///
/// `before` maps a label to `None` when the base-commit run printed no
/// summary, so a check is measured once per run whether or not it counted.
pub fn net_test_counts(
    before: &std::collections::HashMap<String, Option<u32>>,
    after: &std::collections::BTreeMap<String, u32>,
) -> Option<(u32, u32)> {
    let shared: Vec<(u32, u32)> = after
        .iter()
        .filter_map(|(label, a)| before.get(label).copied().flatten().map(|b| (b, *a)))
        .collect();
    if shared.is_empty() {
        return None;
    }
    Some(shared.iter().fold((0, 0), |(b, a), (x, y)| (b + x, a + y)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn empty_checks_are_not_green() {
        let results: Vec<AcceptanceResult> = Vec::new();
        assert!(!all_green(&results));
    }

    #[test]
    fn single_passing_check_is_green() {
        let r = vec![AcceptanceResult::ok("x", "")];
        assert!(all_green(&r));
    }

    #[test]
    fn any_failure_breaks_green() {
        let r = vec![
            AcceptanceResult::ok("x", ""),
            AcceptanceResult::fail("y", "boom"),
            AcceptanceResult::ok("z", ""),
        ];
        assert!(!all_green(&r));
    }

    #[test]
    fn shell_check_passes_for_zero_exit() {
        let dir = tempdir().unwrap();
        let cmd = if cfg!(windows) { "exit 0" } else { "true" };
        let checks = vec![Acceptance::Shell { cmd: cmd.into() }];
        let results = run_acceptance_checks(&checks, dir.path());
        assert!(results[0].ok, "expected ok, got {:?}", results[0]);
    }

    #[test]
    fn shell_check_fails_for_nonzero_exit() {
        let dir = tempdir().unwrap();
        let cmd = if cfg!(windows) { "exit 1" } else { "false" };
        let checks = vec![Acceptance::Shell { cmd: cmd.into() }];
        let results = run_acceptance_checks(&checks, dir.path());
        assert!(!results[0].ok);
        assert!(results[0].output.contains("exit"));
    }

    #[test]
    fn grep_finds_substring() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.rs"),
            b"fn main() {\n    println!(\"--version-only\");\n}\n",
        )
        .unwrap();
        let checks = vec![Acceptance::Grep {
            pattern: "--version-only".into(),
            path: "main.rs".into(),
        }];
        let results = run_acceptance_checks(&checks, dir.path());
        assert!(results[0].ok);
        assert!(results[0].output.contains("--version-only"));
    }

    /// The pattern that broke a live `auto_dispatch` run (#34), verbatim.
    ///
    /// The planner wrote a regex — anchors, `.*`, alternation — into a check
    /// called `grep`. Substring matching looked for it character-for-character,
    /// never found it, and the task failed twice more on the retry ladder
    /// proving the same thing.
    #[test]
    fn grep_handles_the_regex_a_planner_actually_writes() {
        let dir = tempdir().unwrap();
        let file = "lib.rs";
        std::fs::write(
            dir.path().join(file),
            "some preamble
             /// Default number of tasks the daemon may auto-dispatch per cycle.
             fn default_max_auto_dispatch_per_cycle() -> usize {
    1
}
",
        )
        .unwrap();

        let checks = vec![Acceptance::Grep {
            pattern:
                "^///.*default_max_auto_dispatch_per_cycle|fn default_max_auto_dispatch_per_cycle"
                    .into(),
            path: file.into(),
        }];
        let results = run_acceptance_checks(&checks, dir.path());
        assert!(
            results[0].ok,
            "a grep check must accept a grep pattern: {}",
            results[0].output
        );
    }

    /// `^` means start of line, as it does in grep. Anchoring to the start of
    /// the file only would satisfy the syntax and miss the point.
    #[test]
    fn grep_anchors_per_line_not_per_file() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("f.txt"),
            "first line
/// the doc comment
",
        )
        .unwrap();
        let checks = vec![Acceptance::Grep {
            pattern: "^/// the doc".into(),
            path: "f.txt".into(),
        }];
        assert!(run_acceptance_checks(&checks, dir.path())[0].ok);
    }

    /// Literal patterns keep working, including ones that are not valid regex.
    /// Nothing that passed before this change may start failing.
    #[test]
    fn grep_still_matches_literals_that_are_broken_regex() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("f.rs"),
            "fn default_intake_dir() -> String {
",
        )
        .unwrap();
        // `(` unclosed — not a compilable regex, but a real substring.
        let checks = vec![Acceptance::Grep {
            pattern: "fn default_intake_dir(".into(),
            path: "f.rs".into(),
        }];
        assert!(run_acceptance_checks(&checks, dir.path())[0].ok);

        // And a genuine miss still says so, and says the pattern was unusable.
        let miss = vec![Acceptance::Grep {
            pattern: "fn nonexistent(".into(),
            path: "f.rs".into(),
        }];
        let r = run_acceptance_checks(&miss, dir.path());
        assert!(!r[0].ok);
        assert!(r[0].output.contains("not valid regex"), "{}", r[0].output);
    }

    #[test]
    fn grep_misses_substring() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), b"fn main() {}\n").unwrap();
        let checks = vec![Acceptance::Grep {
            pattern: "--version-only".into(),
            path: "main.rs".into(),
        }];
        let results = run_acceptance_checks(&checks, dir.path());
        assert!(!results[0].ok);
        assert!(results[0].output.contains("not found"));
    }

    #[test]
    fn http_check_fails_gracefully_on_unreachable_host() {
        // Offline + deterministic: a closed local port. curl exits non-zero,
        // and the runner surfaces a labeled failure rather than panicking.
        // (The status/body assertion logic is covered by
        // `j6_http_assert_covers_status_string_and_null`.)
        let dir = tempdir().unwrap();
        let checks = vec![Acceptance::Http {
            url: "http://127.0.0.1:1/nope".into(),
            must_match: serde_json::Value::Null,
            schema: None,
        }];
        let results = run_acceptance_checks(&checks, dir.path());
        assert!(!results[0].ok);
        assert!(results[0].label.starts_with("http:"));
    }

    #[test]
    fn run_kind_executes_script() {
        let dir = tempdir().unwrap();
        let cmd = if cfg!(windows) { "exit 0" } else { "true" };
        let checks = vec![Acceptance::Run {
            target: "tui".into(),
            script: Some(cmd.into()),
        }];
        let results = run_acceptance_checks(&checks, dir.path());
        assert!(results[0].ok);
        assert!(results[0].label.starts_with("run: tui"));
    }

    #[test]
    fn assert_passes_when_all_fragments_present() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("shot.svg"), b"<svg>Dark mode on</svg>").unwrap();
        let checks = vec![Acceptance::Assert {
            screenshot: "shot.svg".into(),
            must_contain_text: vec!["Dark mode on".into()],
        }];
        let results = run_acceptance_checks(&checks, dir.path());
        assert!(results[0].ok, "got {:?}", results[0]);
    }

    #[test]
    fn assert_fails_on_missing_text() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("shot.svg"), b"<svg>Light mode</svg>").unwrap();
        let checks = vec![Acceptance::Assert {
            screenshot: "shot.svg".into(),
            must_contain_text: vec!["Dark mode on".into()],
        }];
        let results = run_acceptance_checks(&checks, dir.path());
        assert!(!results[0].ok);
        assert!(results[0].output.contains("missing text"));
    }

    #[test]
    fn assert_fails_on_missing_file() {
        let dir = tempdir().unwrap();
        let checks = vec![Acceptance::Assert {
            screenshot: "nope.svg".into(),
            must_contain_text: vec![],
        }];
        let results = run_acceptance_checks(&checks, dir.path());
        assert!(!results[0].ok);
    }

    #[test]
    fn summarize_counts_green_and_failing() {
        let r = vec![
            AcceptanceResult::ok("a", ""),
            AcceptanceResult::fail("b: bad", ""),
            AcceptanceResult::ok("c", ""),
        ];
        let s = summarize(&r);
        assert!(s.contains("2/3 green"));
        assert!(s.contains("b: bad"));
    }

    /// Regression, from the live `auto_dispatch` run in #34.
    ///
    /// A `cargo check` that blew the per-check timeout was summarised as
    /// `failing: shell: cargo check -p wingman-config` — the same sentence a
    /// genuine compile error produces. The run was recorded as if the code did
    /// not build, when the tree built fine and the cap was simply too small.
    #[test]
    fn summarize_says_why_a_check_failed() {
        let timed_out = vec![
            AcceptanceResult::ok("grep: doc comment", "found"),
            AcceptanceResult::fail("shell: cargo check", "timed out after 600s"),
        ];
        let s = summarize(&timed_out);
        assert!(s.contains("1/2 green"), "{s}");
        assert!(
            s.contains("timed out"),
            "a timeout must be distinguishable from a failing command: {s}"
        );

        // And the other way: a real non-zero exit still reads as one.
        let broke = vec![AcceptanceResult::fail(
            "shell: cargo check",
            "exit 101
error[E0425]: cannot find value",
        )];
        let s = summarize(&broke);
        assert!(s.contains("exit 101"), "{s}");
        assert!(!s.contains("timed out"), "{s}");
    }

    /// The budget is a deadline shared by the whole set, not an allowance each
    /// check gets in full — otherwise `n` slow checks multiply into `n *
    /// budget` and outlive the task that owns them.
    #[test]
    fn the_acceptance_budget_is_shared_not_per_check() {
        let dir = tempdir().unwrap();
        let sleep = if cfg!(windows) {
            "ping -n 6 127.0.0.1 >NUL"
        } else {
            "sleep 5"
        };
        // Four slow checks against a 2s budget. Sharing the deadline costs
        // ~2s in total; handing each check the full budget costs ~8s. The
        // assertion sits between those with room on both sides, so it is a
        // real discriminator rather than a stopwatch.
        let checks = vec![
            Acceptance::Shell { cmd: sleep.into() },
            Acceptance::Shell { cmd: sleep.into() },
            Acceptance::Shell { cmd: sleep.into() },
            Acceptance::Shell { cmd: sleep.into() },
        ];
        let started = std::time::Instant::now();
        let results = run_acceptance_checks_within(&checks, dir.path(), Duration::from_secs(2));
        let elapsed = started.elapsed();

        assert_eq!(results.len(), 4);
        assert!(
            elapsed < Duration::from_secs(5),
            "four checks shared a 2s budget but took {elapsed:?} —              each one was given the whole budget instead of what was left"
        );
        assert!(results.iter().any(|r| !r.ok), "expected a timeout");
    }

    #[test]
    fn j6_http_assert_covers_status_string_and_null() {
        use serde_json::json;
        let lbl = || "http: x".to_string();
        // number → exact status
        assert!(assert_http(lbl(), 200, "hi", &json!(200)).ok);
        assert!(!assert_http(lbl(), 404, "hi", &json!(200)).ok);
        // null → any 2xx/3xx passes, 4xx/5xx fails
        assert!(assert_http(lbl(), 204, "", &json!(null)).ok);
        assert!(!assert_http(lbl(), 500, "", &json!(null)).ok);
        // string → body must contain it AND status < 400
        assert!(assert_http(lbl(), 200, "welcome home", &json!("welcome")).ok);
        assert!(!assert_http(lbl(), 200, "welcome home", &json!("missing")).ok);
        assert!(!assert_http(lbl(), 503, "welcome home", &json!("welcome")).ok);
    }

    /// J6 — the JSON-schema option: the body must be JSON and validate.
    #[test]
    fn j6_http_schema_validates_body_shape() {
        use serde_json::json;
        let passed = || AcceptanceResult::ok("http: x", "status 200");
        let schema = json!({
            "type": "object",
            "required": ["version", "tags"],
            "properties": {
                "version": {"type": "string", "pattern": r"^\d+\.\d+"},
                "tags": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                "port": {"type": "integer", "minimum": 1, "maximum": 65535}
            },
            "additionalProperties": false
        });
        let good = r#"{"version":"0.4.0","tags":["stable"],"port":8080}"#;
        let r = assert_schema(passed(), good, &schema);
        assert!(r.ok, "{}", r.output);

        // Every kind of violation is reported, each with its path.
        let bad = r#"{"tags":[],"port":0.5,"extra":1}"#;
        let r = assert_schema(passed(), bad, &schema);
        assert!(!r.ok);
        for needle in ["`version`", "$.tags", "$.port", "$.extra"] {
            assert!(
                r.output.contains(needle),
                "{needle} missing from {}",
                r.output
            );
        }

        // A body that is not JSON fails rather than vacuously passing.
        assert!(!assert_schema(passed(), "<html>", &schema).ok);

        // Combinators.
        let one_of = json!({"oneOf": [{"type": "string"}, {"type": "integer"}]});
        assert!(assert_schema(passed(), "3", &one_of).ok);
        assert!(!assert_schema(passed(), "true", &one_of).ok);
        let not_enum = json!({"not": {"const": 2}, "enum": [1, 2]});
        assert!(assert_schema(passed(), "1", &not_enum).ok);
        assert!(!assert_schema(passed(), "2", &not_enum).ok);
    }

    /// A schema keyword the validator does not implement must fail the check,
    /// never be skipped: skipping would pass a body nobody actually checked.
    #[test]
    fn j6_http_schema_fails_closed_on_unsupported_keywords() {
        use serde_json::json;
        let passed = AcceptanceResult::ok("http: x", "status 200");
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "annotations are fine",
            "$ref": "#/$defs/thing"
        });
        let r = assert_schema(passed, "{}", &schema);
        assert!(!r.ok);
        assert!(
            r.output.contains("unsupported schema keyword `$ref`"),
            "{}",
            r.output
        );
    }

    /// `schema` is optional in the plan JSON, so existing plans still parse.
    #[test]
    fn j6_http_schema_is_optional_in_plans() {
        let a: Acceptance =
            serde_json::from_str(r#"{"kind":"http","url":"http://x","must_match":200}"#).unwrap();
        assert!(matches!(a, Acceptance::Http { schema: None, .. }));
        let a: Acceptance =
            serde_json::from_str(r#"{"kind":"http","url":"http://x","schema":{"type":"object"}}"#)
                .unwrap();
        assert!(matches!(
            a,
            Acceptance::Http {
                schema: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn j15_passed_tests_reads_each_runner_summary() {
        // cargo test: one summary per test binary, summed.
        let cargo = "running 3 tests\ntest result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\nrunning 9 tests\ntest result: FAILED. 7 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out\n";
        assert_eq!(passed_tests(cargo), Some(10));
        let nextest = "     Summary [   0.123s] 12 tests run: 12 passed, 0 skipped\n";
        assert_eq!(passed_tests(nextest), Some(12));
        let jest = "Test Suites: 1 failed, 2 passed, 3 total\nTests:       1 failed, 12 passed, 13 total\n";
        assert_eq!(passed_tests(jest), Some(12));
        let vitest = " Test Files  3 passed (3)\n      Tests  21 passed (21)\n";
        assert_eq!(passed_tests(vitest), Some(21));
        // Coloured, as pytest prints when it thinks it owns a terminal.
        let pytest = "\x1b[32m===== 1 failed, 4 passed, 1 warning in 0.12s =====\x1b[0m\n";
        assert_eq!(passed_tests(pytest), Some(4));
        let go = "=== RUN   TestA\n--- PASS: TestA (0.00s)\n=== RUN   TestB\n    --- PASS: TestB/sub (0.00s)\n--- FAIL: TestB (0.00s)\n";
        assert_eq!(passed_tests(go), Some(2));
        // No summary at all: no count, never a zero.
        assert_eq!(
            passed_tests("Finished dev profile\nok  \tpkg\t0.01s\n"),
            None
        );
    }

    #[test]
    fn j15_shell_checks_carry_the_whole_outputs_count() {
        let dir = tempfile::tempdir().unwrap();
        // More summary lines than the 1 KiB tail keeps, and a failing exit.
        let line = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out";
        std::fs::write(dir.path().join("out.txt"), format!("{line}\n").repeat(40)).unwrap();
        let cmd = if cfg!(windows) {
            "type out.txt && exit 1"
        } else {
            "cat out.txt; exit 1"
        };
        let r = run_acceptance_checks(&[Acceptance::Shell { cmd: cmd.into() }], dir.path());
        assert!(!r[0].ok);
        assert!(r[0].output.len() < line.len() * 40);
        assert_eq!(r[0].passed_tests, Some(40));
    }

    #[test]
    fn j15_test_checks_are_the_ones_that_run_tests() {
        let shell = |c: &str| Acceptance::Shell { cmd: c.into() };
        assert_eq!(
            test_check_label(&shell("cargo test -p x")).as_deref(),
            Some("shell: cargo test -p x")
        );
        assert_eq!(test_check_label(&shell("cargo check")), None);
        let run = Acceptance::Run {
            target: "suite".into(),
            script: Some("npx jest".into()),
        };
        assert_eq!(test_check_label(&run).as_deref(), Some("run: suite"));
        let grep = Acceptance::Grep {
            pattern: "test".into(),
            path: "x".into(),
        };
        assert_eq!(test_check_label(&grep), None);
    }

    #[test]
    fn j15_net_counts_compare_only_shared_checks() {
        let before: std::collections::HashMap<String, Option<u32>> = [
            ("a".to_string(), Some(10)),
            ("b".to_string(), Some(5)),
            ("c".to_string(), None),
        ]
        .into();
        let after: std::collections::BTreeMap<String, u32> =
            [("a".to_string(), 8), ("c".to_string(), 99)].into();
        assert_eq!(net_test_counts(&before, &after), Some((10, 8)));
        let unrelated: std::collections::BTreeMap<String, u32> = [("c".to_string(), 1)].into();
        assert_eq!(net_test_counts(&before, &unrelated), None);
    }
}
