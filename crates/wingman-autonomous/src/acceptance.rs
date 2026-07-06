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
}

impl AcceptanceResult {
    pub fn ok(label: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ok: true,
            output: output.into(),
        }
    }
    pub fn fail(label: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ok: false,
            output: output.into(),
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
    let mut out = Vec::with_capacity(checks.len());
    for c in checks {
        out.push(run_one(c, cwd));
    }
    out
}

fn run_one(check: &Acceptance, cwd: &Path) -> AcceptanceResult {
    match check {
        Acceptance::Shell { cmd } => run_shell(cmd, cwd),
        Acceptance::Grep { pattern, path } => run_grep(pattern, path, cwd),
        // J6 — real HTTP GET via `curl` (no async runtime, no new dep). The
        // status line proves reachability; `must_match` asserts on body/code.
        Acceptance::Http { url, must_match } => run_http(url, must_match, cwd),
        // J6 — run the app: execute the script (or the target as a
        // command) like a shell check, but label it as a run.
        Acceptance::Run { target, script } => {
            let cmd = script.clone().unwrap_or_else(|| target.clone());
            let mut res = run_shell(&cmd, cwd);
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
/// ponytail: substring/status checks, not a JSON-schema match. Add a real
/// JSON-path assertion when a canned string-contains proves too blunt.
fn run_http(url: &str, must_match: &serde_json::Value, cwd: &Path) -> AcceptanceResult {
    let label = format!("http: {url}");
    // -sS quiet-but-show-errors, -L follow redirects, -m 30 hard timeout,
    // -w appends the numeric status on its own trailing line.
    let output = Command::new("curl")
        .args([
            "-sSL",
            "-m",
            "30",
            "-o",
            "-",
            "-w",
            "\n%{http_code}",
            url,
        ])
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
    assert_http(label, status, body, must_match)
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

const SHELL_TIMEOUT: Duration = Duration::from_secs(60);
const OUTPUT_TAIL_BYTES: usize = 1024;

fn run_shell(cmd: &str, cwd: &Path) -> AcceptanceResult {
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
                if status.success() {
                    return AcceptanceResult::ok(label, tail);
                } else {
                    let code = status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".to_string());
                    return AcceptanceResult::fail(label, format!("exit {code}\n{tail}"));
                }
            }
            Ok(None) => {
                if started.elapsed() > SHELL_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return AcceptanceResult::fail(
                        label,
                        format!("timed out after {:?}", SHELL_TIMEOUT),
                    );
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
    // Plain substring match; the planner uses grep checks as cheap
    // "did the string land in the file?" probes, not full regexes.
    if let Some(idx) = body.find(pattern) {
        // Surface the matching line so the model knows where it hit.
        let line_start = body[..idx].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = body[idx..]
            .find('\n')
            .map(|i| idx + i)
            .unwrap_or(body.len());
        let line = &body[line_start..line_end];
        AcceptanceResult::ok(label, line.to_string())
    } else {
        AcceptanceResult::fail(label, format!("pattern {pattern:?} not found in {path}"))
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
        format!(
            "{}/{total} green; failing: {}",
            total - failed,
            results
                .iter()
                .filter(|r| !r.ok)
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
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
}
