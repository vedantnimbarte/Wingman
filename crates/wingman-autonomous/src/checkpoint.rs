//! E11 — mandatory checkpoint hygiene.
//!
//! The worker system prompt mandates the `checkpoint` tool
//! ([`crate::tools::Checkpoint`]) before any multi-file edit and after each
//! acceptance-green milestone, so a bad turn is recoverable. This module is
//! the *verifier*: with the `checkpoint_hygiene` capability on, the worker
//! supervisor ([`crate::worker::run_worker`]) confirms from the attempt's
//! recorded `task.tool` events that the worker actually checkpointed before
//! it lets the task enter `review`, and fails the attempt otherwise.
//!
//! Two rules:
//!
//! 1. A checkpoint must precede the worker's *second distinct file edit*
//!    (the point at which a single bad turn could corrupt multiple files).
//! 2. At least one checkpoint must exist before `review`.
//!
//! A task that edits zero or one file is exempt from rule 1 (nothing to
//! protect) but a single-file task that ran with no checkpoint at all is
//! allowed — there's nothing multi-file to recover. The strict gate fires
//! only on multi-file work, matching the plan's "before any multi-file
//! edit" wording.

use crate::model::Event;

/// Tool names that mutate files. A second distinct file among these
/// without a preceding checkpoint trips rule 1.
const EDIT_TOOLS: &[&str] = &[
    "edit_file",
    "write_file",
    "apply_patch",
    "create_file",
    "str_replace",
    "multi_edit",
];

/// One observed tool call in a worker's turn sequence.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub tool: String,
    /// File the call touched, when the tool reports one.
    pub file: Option<String>,
}

fn is_checkpoint(tool: &str) -> bool {
    tool.contains("checkpoint")
}

fn is_edit(tool: &str) -> bool {
    EDIT_TOOLS.contains(&tool)
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckpointVerdict {
    /// Hygiene satisfied — the task may enter `review`.
    Ok,
    /// A rule was violated; `reason` explains which.
    Violation { reason: String },
}

impl CheckpointVerdict {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// Verify checkpoint hygiene over a worker's ordered tool calls.
pub fn verify(calls: &[ToolCall]) -> CheckpointVerdict {
    let mut seen_checkpoint = false;
    let mut edited_files: Vec<String> = Vec::new();
    let mut distinct_edits = 0usize;

    for call in calls {
        if is_checkpoint(&call.tool) {
            seen_checkpoint = true;
            continue;
        }
        if is_edit(&call.tool) {
            // Count distinct files (an unnamed edit counts as its own
            // distinct touch — conservative).
            let is_new = match &call.file {
                Some(f) => {
                    if edited_files.iter().any(|e| e == f) {
                        false
                    } else {
                        edited_files.push(f.clone());
                        true
                    }
                }
                None => true,
            };
            if is_new {
                distinct_edits += 1;
                // Rule 1: the second distinct edit must be preceded by a
                // checkpoint.
                if distinct_edits == 2 && !seen_checkpoint {
                    return CheckpointVerdict::Violation {
                        reason: "second file edited before any checkpoint (E11 rule 1)".to_string(),
                    };
                }
            }
        }
    }

    // Rule 2: multi-file work must have checkpointed at least once.
    if distinct_edits >= 2 && !seen_checkpoint {
        return CheckpointVerdict::Violation {
            reason: "multi-file task reached review with no checkpoint (E11 rule 2)".to_string(),
        };
    }

    CheckpointVerdict::Ok
}

/// Extract the tool calls (in order) of a task's latest attempt from a slice
/// of run events: those recorded since its last `task.assign`, or all of them
/// when it was never assigned. An earlier attempt's edits say nothing about
/// whether this one checkpointed.
pub fn tool_calls_for_task(events: &[Event], task_id: &str) -> Vec<ToolCall> {
    let since = events
        .iter()
        .rposition(|e| matches!(e, Event::TaskAssign { id, .. } if id == task_id))
        .map_or(0, |i| i + 1);
    events[since..]
        .iter()
        .filter_map(|e| match e {
            Event::TaskTool { id, tool, file, .. } if id == task_id => Some(ToolCall {
                tool: tool.clone(),
                // Now populated from the tool's `path` input, so multi-*file*
                // work is distinguished from a single file edited by several
                // tool calls (which must not trip the multi-file gate).
                file: file.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// E5.5 — per-turn rollback to the last green checkpoint.
///
/// Wraps the worker's turn gate. Every time the gate passes, the worktree's
/// files are captured as that checkpoint ([`crate::worktree::snapshot_tree`]).
/// When the gate fails `after` times in a row, the worktree is restored to it
/// and the model is told its edits since then are gone, rather than being
/// asked, yet again, to repair a tree it has already failed to repair.
///
/// Before the gate has ever passed, the tree the worker started from is the
/// candidate: it is restored and re-checked, and if it fails the gate too
/// (the base itself is red) the worker's edits are put back, since there is
/// nothing green to return to.
pub struct RollbackGate {
    inner: std::sync::Arc<dyn wingman_core::TurnGate>,
    root: std::path::PathBuf,
    after: u32,
    state: tokio::sync::Mutex<RollbackState>,
}

#[derive(Default)]
struct RollbackState {
    /// The worktree's tree the last time the gate passed.
    green: Option<String>,
    /// The tree the worker started from, until it is proven red.
    start: Option<String>,
    /// Consecutive gate failures since the last pass or rollback.
    failures: u32,
}

impl RollbackGate {
    /// Capture the starting tree of the worktree at `root`. Rolls back after
    /// `after` consecutive failures (at least one).
    pub fn new(
        inner: std::sync::Arc<dyn wingman_core::TurnGate>,
        root: std::path::PathBuf,
        after: u32,
    ) -> Self {
        let start = crate::worktree::snapshot_tree(&root)
            .map_err(|e| tracing::warn!(target: "pilot::rollback", "no starting checkpoint: {e}"))
            .ok();
        Self {
            inner,
            root,
            after: after.max(1),
            state: tokio::sync::Mutex::new(RollbackState {
                start,
                ..Default::default()
            }),
        }
    }

    async fn snapshot(&self) -> Result<String, String> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || crate::worktree::snapshot_tree(&root))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }

    async fn restore(&self, tree: String) -> Result<String, String> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || crate::worktree::restore_tree(&root, &tree))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }

    /// Restore the last green checkpoint and say what happened, for the model.
    async fn roll_back(&self, state: &mut RollbackState) -> String {
        let rolled_back = format!(
            "[wingman rollback] The gate failed {} times in a row, so the worktree was \
             restored to the last state that passed it. Every edit since then is gone: \
             re-read files before editing them, and take a different approach from the \
             one that failed.",
            self.after
        );
        if let Some(green) = state.green.clone() {
            return match self.restore(green).await {
                Ok(_) => rolled_back,
                Err(e) => {
                    format!("[wingman rollback] could not restore the last green checkpoint: {e}")
                }
            };
        }
        let Some(start) = state.start.clone() else {
            return "[wingman rollback] there is no green checkpoint to return to; your edits \
                    are kept."
                .into();
        };
        let broken = match self.restore(start.clone()).await {
            Ok(broken) => broken,
            Err(e) => {
                return format!("[wingman rollback] could not restore the starting tree: {e}")
            }
        };
        if self.inner.check().await.passed {
            state.green = Some(start);
            return rolled_back;
        }
        // The base is red too. Put the worker's edits back and stop trying.
        state.start = None;
        if let Err(e) = self.restore(broken).await {
            return format!(
                "[wingman rollback] could not put your edits back after checking the starting \
                 tree: {e}"
            );
        }
        "[wingman rollback] the tree this task started from fails the gate as well, so there is \
         no green checkpoint to return to; your edits are kept."
            .into()
    }
}

#[async_trait::async_trait]
impl wingman_core::TurnGate for RollbackGate {
    fn label(&self) -> String {
        self.inner.label()
    }

    async fn check(&self) -> wingman_core::GateReport {
        let report = self.inner.check().await;
        let mut state = self.state.lock().await;
        if report.passed {
            state.failures = 0;
            match self.snapshot().await {
                Ok(tree) => state.green = Some(tree),
                Err(e) => tracing::warn!(target: "pilot::rollback", "checkpoint not captured: {e}"),
            }
            return report;
        }
        state.failures += 1;
        if state.failures < self.after {
            return report;
        }
        state.failures = 0;
        let note = self.roll_back(&mut state).await;
        tracing::warn!(target: "pilot::rollback", "{note}");
        wingman_core::GateReport {
            passed: false,
            summary: format!("{}\n\n{note}", report.summary),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(tool: &str, file: Option<&str>) -> ToolCall {
        ToolCall {
            tool: tool.into(),
            file: file.map(String::from),
        }
    }

    #[test]
    fn single_file_edit_without_checkpoint_is_ok() {
        let calls = vec![call("edit_file", Some("a.rs"))];
        assert!(verify(&calls).is_ok());
    }

    #[test]
    fn multi_file_with_checkpoint_first_is_ok() {
        let calls = vec![
            call("checkpoint", None),
            call("edit_file", Some("a.rs")),
            call("edit_file", Some("b.rs")),
        ];
        assert!(verify(&calls).is_ok());
    }

    #[test]
    fn checkpoint_between_edits_satisfies_rule_one() {
        let calls = vec![
            call("edit_file", Some("a.rs")),
            call("checkpoint", None),
            call("edit_file", Some("b.rs")),
        ];
        assert!(verify(&calls).is_ok());
    }

    #[test]
    fn second_edit_before_checkpoint_violates() {
        let calls = vec![
            call("edit_file", Some("a.rs")),
            call("edit_file", Some("b.rs")),
            call("checkpoint", None),
        ];
        let v = verify(&calls);
        assert!(!v.is_ok());
        if let CheckpointVerdict::Violation { reason } = v {
            assert!(reason.contains("rule 1"));
        }
    }

    #[test]
    fn re_editing_same_file_is_not_a_second_distinct_edit() {
        let calls = vec![
            call("edit_file", Some("a.rs")),
            call("edit_file", Some("a.rs")),
            call("edit_file", Some("a.rs")),
        ];
        assert!(verify(&calls).is_ok());
    }

    #[test]
    fn unnamed_edits_count_as_distinct() {
        // Two file-less edits → treated as multi-file, needs checkpoint.
        let calls = vec![call("apply_patch", None), call("apply_patch", None)];
        assert!(!verify(&calls).is_ok());
    }

    #[test]
    fn non_edit_tools_are_ignored() {
        let calls = vec![
            call("read_file", Some("a.rs")),
            call("grep_tool", None),
            call("list_dir", None),
            call("edit_file", Some("a.rs")),
        ];
        assert!(verify(&calls).is_ok());
    }

    #[test]
    fn same_file_via_two_different_edit_tools_is_ok() {
        // Regression: a single-file change made with `edit_file` then
        // `write_file` (same path) must not trip the multi-file gate just
        // because two tools touched it.
        let calls = vec![
            call("edit_file", Some("README.md")),
            call("write_file", Some("README.md")),
        ];
        assert!(verify(&calls).is_ok());
    }

    /* ── E5.5 rollback gate ─────────────────────────────────────────────── */

    /// A gate that answers from a script.
    struct ScriptedGate(std::sync::Mutex<std::collections::VecDeque<bool>>);

    #[async_trait::async_trait]
    impl wingman_core::TurnGate for ScriptedGate {
        fn label(&self) -> String {
            "scripted".into()
        }
        async fn check(&self) -> wingman_core::GateReport {
            let passed = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("unscripted check");
            wingman_core::GateReport {
                passed,
                summary: if passed { "ok" } else { "red" }.into(),
            }
        }
    }

    /// A one-commit repo holding `lib.rs`, or `None` without git.
    fn repo_with(file: &str) -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
        };
        git(&["init", "-q"]).ok()?;
        for kv in [
            ["user.email", "t@t.t"],
            ["user.name", "t"],
            ["core.autocrlf", "false"],
        ] {
            git(&["config", kv[0], kv[1]]).unwrap();
        }
        std::fs::write(dir.path().join("lib.rs"), file).unwrap();
        git(&["add", "-A"]).unwrap();
        git(&["commit", "-qm", "base"]).unwrap();
        Some(dir)
    }

    fn gate(dir: &std::path::Path, script: &[bool], after: u32) -> RollbackGate {
        RollbackGate::new(
            std::sync::Arc::new(ScriptedGate(std::sync::Mutex::new(
                script.iter().copied().collect(),
            ))),
            dir.to_path_buf(),
            after,
        )
    }

    #[tokio::test]
    async fn repeated_gate_failures_roll_back_to_the_last_green_checkpoint() {
        use wingman_core::TurnGate;
        let Some(dir) = repo_with("base\n") else {
            eprintln!("skipping: git not available");
            return;
        };
        let read = || std::fs::read_to_string(dir.path().join("lib.rs")).unwrap();
        let g = gate(dir.path(), &[true, false, false], 2);

        std::fs::write(dir.path().join("lib.rs"), "green\n").unwrap();
        assert!(g.check().await.passed);

        std::fs::write(dir.path().join("lib.rs"), "broken\n").unwrap();
        std::fs::write(dir.path().join("extra.rs"), "worse\n").unwrap();
        // The first failure is only reported.
        let first = g.check().await;
        assert!(!first.passed && !first.summary.contains("rollback"));
        assert_eq!(read(), "broken\n");
        // The second rolls back and says so.
        let second = g.check().await;
        assert!(!second.passed);
        assert!(
            second
                .summary
                .contains("restored to the last state that passed"),
            "{}",
            second.summary
        );
        assert_eq!(read(), "green\n");
        assert!(!dir.path().join("extra.rs").exists());
    }

    #[tokio::test]
    async fn before_any_pass_the_starting_tree_is_used_only_if_it_is_green() {
        use wingman_core::TurnGate;
        let Some(dir) = repo_with("base\n") else {
            eprintln!("skipping: git not available");
            return;
        };
        let read = || std::fs::read_to_string(dir.path().join("lib.rs")).unwrap();

        // Red twice, then the restored base passes: keep the base.
        let g = gate(dir.path(), &[false, false, true], 2);
        std::fs::write(dir.path().join("lib.rs"), "broken\n").unwrap();
        g.check().await;
        assert!(g.check().await.summary.contains("restored"));
        assert_eq!(read(), "base\n");

        // Red twice, and the base is red as well: the edits come back.
        let g = gate(dir.path(), &[false, false, false], 2);
        std::fs::write(dir.path().join("lib.rs"), "wip\n").unwrap();
        g.check().await;
        let report = g.check().await;
        assert!(
            report.summary.contains("your edits are kept"),
            "{}",
            report.summary
        );
        assert_eq!(read(), "wip\n");
    }

    #[test]
    fn tool_calls_for_task_filters_by_id_and_carries_file() {
        let events = vec![
            Event::TaskTool {
                t: "t".into(),
                id: "t1".into(),
                agent: "a".into(),
                tool: "edit_file".into(),
                input_hash: None,
                file: Some("a.rs".into()),
                ok: true,
            },
            Event::TaskTool {
                t: "t".into(),
                id: "t2".into(),
                agent: "a".into(),
                tool: "checkpoint".into(),
                input_hash: None,
                file: None,
                ok: true,
            },
            Event::TaskTool {
                t: "t".into(),
                id: "t1".into(),
                agent: "a".into(),
                tool: "checkpoint".into(),
                input_hash: None,
                file: None,
                ok: true,
            },
        ];
        let calls = tool_calls_for_task(&events, "t1");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tool, "edit_file");
        assert_eq!(calls[0].file.as_deref(), Some("a.rs"));
        assert_eq!(calls[1].tool, "checkpoint");
    }

    /// A retry that checkpointed is not failed for the attempt before it.
    #[test]
    fn tool_calls_for_task_reads_only_the_latest_attempt() {
        let tool = |name: &str, file: Option<&str>| Event::TaskTool {
            t: "t".into(),
            id: "t1".into(),
            agent: "a".into(),
            tool: name.into(),
            input_hash: None,
            file: file.map(String::from),
            ok: true,
        };
        let assign = |agent: &str| Event::TaskAssign {
            t: "t".into(),
            id: "t1".into(),
            agent: agent.into(),
            worktree: "wt".into(),
        };
        let events = vec![
            assign("a1"),
            tool("edit_file", Some("a.rs")),
            tool("edit_file", Some("b.rs")),
            assign("a2"),
            tool("checkpoint", None),
            tool("edit_file", Some("a.rs")),
            tool("edit_file", Some("b.rs")),
        ];
        let calls = tool_calls_for_task(&events, "t1");
        assert_eq!(calls.len(), 3);
        assert!(verify(&calls).is_ok());
        assert!(!verify(&tool_calls_for_task(&events[..3], "t1")).is_ok());
    }
}
