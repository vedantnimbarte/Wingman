//! E11 — mandatory checkpoint hygiene.
//!
//! The worker system prompt mandates `wingman checkpoint` before any
//! multi-file edit and after each acceptance-green milestone, so a bad
//! turn is recoverable (and the E5 turn-gate has a snapshot to roll back
//! to). This module is the orchestrator's *verifier*: before a task is
//! allowed to enter `review`, confirm — from the recorded `task.tool`
//! event stream — that the worker actually checkpointed.
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

/// Extract a task's tool calls (in order) from a slice of run events.
pub fn tool_calls_for_task(events: &[Event], task_id: &str) -> Vec<ToolCall> {
    events
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
}
