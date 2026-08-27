//! `run_plan` end to end, against the real registry.
//!
//! The unit tests in `run_plan.rs` cover substitution and capture. These cover
//! the two claims that actually matter: that a plan cannot do what the session
//! is not allowed to do, and that it does the one thing it exists for —
//! feeding an earlier call's output into a later call's arguments without a
//! second round trip.

use std::sync::Arc;

use serde_json::json;
use wingman_config::PermissionMode;
use wingman_core::ToolDispatcher;
use wingman_tools::builtin::RunPlan;
use wingman_tools::{ToolCtx, ToolRegistry};

/// A registry with `run_plan` wired the way `runtime.rs` wires it.
fn registry(mode: PermissionMode, root: &std::path::Path) -> Arc<ToolRegistry> {
    let ctx = ToolCtx::new(mode, root.to_path_buf(), root.to_path_buf());
    let reg = Arc::new(ToolRegistry::new(ctx).with_builtins());
    let as_dispatcher: Arc<dyn ToolDispatcher> = reg.clone();
    reg.register_arc(Arc::new(RunPlan::new(Arc::downgrade(&as_dispatcher))));
    reg
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wingman-run-plan-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// The security claim. A plan is dispatched call by call through the same
/// gate as any other tool call, so read-only mode has to refuse the write —
/// otherwise `run_plan` would be a way to launder a forbidden call through an
/// allowed one.
#[tokio::test]
async fn a_plan_cannot_write_in_read_only_mode() {
    let dir = tempdir("readonly");
    let target = dir.join("should-not-exist.txt");
    let reg = registry(PermissionMode::ReadOnly, &dir);

    let out = reg
        .dispatch(
            "run_plan",
            json!({"steps": [
                {"tool": "write_file",
                 "args": {"path": target.to_string_lossy(), "content": "escaped"}}
            ]}),
        )
        .await;

    assert!(
        !target.exists(),
        "read-only mode let a plan write {}",
        target.display()
    );
    assert!(
        out.content.contains("(error)"),
        "the denial should be visible in the plan output, got: {}",
        out.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The reason the tool exists: step 1's arguments come out of step 0's output,
/// in one dispatch. Without it this costs two round trips, because the model
/// cannot write the `read_file` calls until it has seen the grep results.
#[tokio::test]
async fn for_each_feeds_an_earlier_steps_output_into_a_later_call() {
    let dir = tempdir("dataflow");
    std::fs::write(dir.join("a.rs"), "fn a() { NEEDLE }\n").unwrap();
    std::fs::write(dir.join("b.rs"), "fn b() { NEEDLE }\n").unwrap();
    std::fs::write(dir.join("c.rs"), "fn c() { unrelated }\n").unwrap();
    let reg = registry(PermissionMode::ReadOnly, &dir);

    let out = reg
        .dispatch(
            "run_plan",
            json!({"steps": [
                {"tool": "grep", "args": {"pattern": "NEEDLE"}},
                {"tool": "read_file",
                 "args": {"path": "{}"},
                 "for_each": {"step": 0, "capture": "^([^:]+):"}}
            ]}),
        )
        .await;

    assert!(!out.is_error, "plan failed: {}", out.content);
    // Both matching files were read; the non-matching one was not.
    assert!(
        out.content.contains("fn a()"),
        "missing a.rs: {}",
        out.content
    );
    assert!(
        out.content.contains("fn b()"),
        "missing b.rs: {}",
        out.content
    );
    assert!(
        !out.content.contains("fn c()"),
        "read a file grep did not match: {}",
        out.content
    );
    // grep + two reads.
    assert!(
        out.content.contains("[run_plan: 3 call(s)]"),
        "expected 3 calls: {}",
        out.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Nesting would make every bound on this tool meaningless: fan-out becomes
/// exponential and the call cap only limits one level.
#[tokio::test]
async fn a_plan_cannot_contain_a_plan() {
    let dir = tempdir("recursion");
    let reg = registry(PermissionMode::ReadOnly, &dir);

    let out = reg
        .dispatch(
            "run_plan",
            json!({"steps": [{"tool": "run_plan", "args": {"steps": []}}]}),
        )
        .await;

    assert!(out.is_error, "nesting was allowed: {}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `for_each` must look backwards. Referring to the current or a later step
/// would read an output that does not exist yet.
#[tokio::test]
async fn for_each_must_reference_an_earlier_step() {
    let dir = tempdir("order");
    let reg = registry(PermissionMode::ReadOnly, &dir);

    let out = reg
        .dispatch(
            "run_plan",
            json!({"steps": [
                {"tool": "list_dir", "args": {"path": "."},
                 "for_each": {"step": 0, "capture": "^(.+)$"}}
            ]}),
        )
        .await;

    assert!(out.is_error, "a self-referential step ran: {}", out.content);
    assert!(out.content.contains("EARLIER"), "got: {}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A step that fails stops the plan, because later steps are usually written
/// against its output and would otherwise fan out over an error message.
#[tokio::test]
async fn a_failed_step_stops_the_plan() {
    let dir = tempdir("failstop");
    let reg = registry(PermissionMode::ReadOnly, &dir);

    let out = reg
        .dispatch(
            "run_plan",
            json!({"steps": [
                {"tool": "read_file", "args": {"path": "no-such-file.txt"}},
                {"tool": "list_dir", "args": {"path": "."}}
            ]}),
        )
        .await;

    assert!(
        out.content.contains("later steps were not run"),
        "expected the plan to stop: {}",
        out.content
    );
    assert!(
        out.content.contains("[run_plan: 1 call(s)]"),
        "the second step should not have run: {}",
        out.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}
