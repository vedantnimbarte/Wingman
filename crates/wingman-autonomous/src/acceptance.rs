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
        // status line proves reachability; `must_match` asserts on body/code.
        Acceptance::Http { url, must_match } => run_http(url, must_match, cwd),
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
/// ponytail: substring/status checks, not a JSON-schema match. Add a real
/// JSON-path assertion when a canned string-contains proves too blunt.
fn run_http(url: &str, must_match: &serde_json::Value, cwd: &Path) -> AcceptanceResult {
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
}
