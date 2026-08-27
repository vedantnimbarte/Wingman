//! Background jobs, end to end against real processes.
//!
//! The unit tests in `jobs.rs` cover the buffer; these cover the part that
//! only shows up with an actual child: that a job runs without blocking, that
//! its output is collected, that it is reported as finished once it exits,
//! and that stopping it works.

use serde_json::json;
use wingman_config::PermissionMode;
use wingman_core::ToolDispatcher;
use wingman_tools::{ToolCtx, ToolRegistry};

fn registry() -> ToolRegistry {
    let tmp = std::env::temp_dir();
    // Yolo so the shell capability is granted; this is about job mechanics,
    // and the permission gate has its own tests.
    ToolRegistry::new(ToolCtx::new(PermissionMode::Yolo, tmp.clone(), tmp)).with_builtins()
}

/// Portable "sleep then print" — the shells differ, so the command does too.
fn slow_command() -> &'static str {
    if cfg!(windows) {
        "ping -n 4 127.0.0.1 > NUL && echo DONE-MARKER"
    } else {
        "sleep 3; echo DONE-MARKER"
    }
}

fn echo_command() -> &'static str {
    "echo HELLO-MARKER"
}

async fn job_id_from(out: &str) -> String {
    out.lines()
        .next()
        .and_then(|l| l.strip_prefix("started "))
        .unwrap_or_else(|| panic!("expected a job id, got: {out}"))
        .trim()
        .to_string()
}

#[tokio::test]
async fn a_background_job_returns_immediately_and_then_reports_its_output() {
    let reg = registry();
    let started = std::time::Instant::now();
    let out = reg
        .dispatch(
            "run_shell",
            json!({ "command": echo_command(), "background": true }),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    // The point of the feature: control comes back without waiting for the
    // process. Generous bound — this is asserting "did not block", not a
    // performance target.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "starting a background job blocked for {:?}",
        started.elapsed()
    );

    let id = job_id_from(&out.content).await;

    // Give the child time to run and the drain task time to see it.
    let mut collected = String::new();
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let res = reg.dispatch("job_output", json!({ "id": id })).await;
        collected = res.content;
        if collected.contains("HELLO-MARKER") && collected.contains("exited") {
            break;
        }
    }
    assert!(
        collected.contains("HELLO-MARKER"),
        "job output never arrived: {collected}"
    );
    assert!(
        collected.contains("exited"),
        "a finished job should not still read as running: {collected}"
    );
}

#[tokio::test]
async fn a_running_job_is_listed_then_stopped() {
    let reg = registry();
    let out = reg
        .dispatch(
            "run_shell",
            json!({ "command": slow_command(), "background": true }),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    let id = job_id_from(&out.content).await;

    let listed = reg.dispatch("job_list", json!({})).await;
    assert!(listed.content.contains(&id), "{}", listed.content);
    assert!(
        listed.content.contains("running"),
        "a just-started slow job should be running: {}",
        listed.content
    );

    let stopped = reg.dispatch("job_stop", json!({ "id": id })).await;
    assert!(!stopped.is_error, "{}", stopped.content);

    let after = reg.dispatch("job_output", json!({ "id": id })).await;
    assert!(
        after.content.contains("killed"),
        "a stopped job should report killed: {}",
        after.content
    );
}

#[tokio::test]
async fn job_tools_refuse_an_unknown_id_rather_than_panicking() {
    let reg = registry();
    for tool in ["job_output", "job_stop"] {
        let out = reg
            .dispatch(tool, json!({ "id": "job-does-not-exist" }))
            .await;
        assert!(out.is_error, "{tool} should refuse an unknown id");
        assert!(out.content.contains("no such job"), "{}", out.content);
    }
}

#[tokio::test]
async fn with_no_jobs_the_list_says_so() {
    let reg = registry();
    let out = reg.dispatch("job_list", json!({})).await;
    assert!(!out.is_error);
    assert!(
        out.content.contains("no background jobs"),
        "{}",
        out.content
    );
}

/// A background command is not a less-guarded command: it goes through the
/// same permission gate as a foreground one.
#[tokio::test]
async fn a_background_job_is_refused_in_read_only_mode() {
    let tmp = std::env::temp_dir();
    let reg =
        ToolRegistry::new(ToolCtx::new(PermissionMode::ReadOnly, tmp.clone(), tmp)).with_builtins();
    let out = reg
        .dispatch(
            "run_shell",
            json!({ "command": echo_command(), "background": true }),
        )
        .await;
    assert!(out.is_error, "read-only must refuse a background shell too");
}

/// The capability P11 was really about: hold a process open and drive it
/// across several tool calls, rather than one-shot each command.
///
/// Asserted through *state*, not output. A child's stdout buffering is its
/// own business — `findstr` block-buffers on a pipe, `cat` full-buffers — so
/// a test that waits for echoed text measures the child's libc rather than
/// this feature. A process that reads two lines exits only after the second
/// arrives, which is precisely the property being claimed: it was still alive
/// between two separate tool calls.
#[tokio::test]
async fn a_job_stays_alive_between_sends() {
    let reg = registry();
    // Two blocking reads, then exit. No variable expansion: on Windows
    // `%A%` is substituted when the line is parsed, before `set /p` runs.
    let command = if cfg!(windows) {
        "set /p A= && set /p B="
    } else {
        "read A; read B"
    };
    let out = reg
        .dispatch(
            "run_shell",
            json!({ "command": command, "background": true }),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    let id = job_id_from(&out.content).await;

    let state = reg.dispatch("job_output", json!({ "id": id })).await;
    assert!(state.content.contains("running"), "{}", state.content);

    // One line satisfies the first read; the second still blocks, so the
    // process must still be alive.
    let sent = reg
        .dispatch("job_send", json!({ "id": id, "input": "ALPHA" }))
        .await;
    assert!(!sent.is_error, "{}", sent.content);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let mid = reg.dispatch("job_output", json!({ "id": id })).await;
    assert!(
        mid.content.contains("running"),
        "the job should still be waiting on its second read: {}",
        mid.content
    );

    // A separate tool call against the same live process. Only now finishes.
    let sent = reg
        .dispatch("job_send", json!({ "id": id, "input": "BETA" }))
        .await;
    assert!(!sent.is_error, "{}", sent.content);

    let mut final_state = String::new();
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        final_state = reg
            .dispatch("job_output", json!({ "id": id }))
            .await
            .content;
        if final_state.contains("exited") {
            break;
        }
    }
    assert!(
        final_state.contains("exited"),
        "the job never consumed the second line: {final_state}"
    );
}

#[tokio::test]
async fn sending_to_a_finished_job_says_so_rather_than_failing_obscurely() {
    let reg = registry();
    let out = reg
        .dispatch(
            "run_shell",
            json!({ "command": echo_command(), "background": true }),
        )
        .await;
    let id = job_id_from(&out.content).await;

    // Wait for it to exit.
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let o = reg.dispatch("job_output", json!({ "id": id })).await;
        if o.content.contains("exited") {
            break;
        }
    }
    let sent = reg
        .dispatch("job_send", json!({ "id": id, "input": "too late" }))
        .await;
    assert!(sent.is_error);
    assert!(
        sent.content.contains("nothing is listening"),
        "{}",
        sent.content
    );
}

#[tokio::test]
async fn sending_to_an_unknown_job_is_refused() {
    let reg = registry();
    let out = reg
        .dispatch("job_send", json!({ "id": "job-nope", "input": "x" }))
        .await;
    assert!(out.is_error);
    assert!(out.content.contains("no such job"), "{}", out.content);
}
