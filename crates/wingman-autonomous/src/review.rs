//! E7 — per-task reviewer (replaces the end-of-run reviewer).
//!
//! When a worker reports `review` with green acceptance, the orchestrator
//! immediately spawns a read-only reviewer agent on *that one task's*
//! diff, in parallel with the next eligible worker. Catching issues at the
//! per-task level — the cheapest possible point — is what lets the human
//! PR review become a rubber stamp.
//!
//! This module is the decision core: parse the reviewer agent's JSON
//! verdict, and decide the task's next status. The orchestrator owns
//! spawning and merging; a final cross-cutting reviewer still runs on the
//! integration branch for changelog/release-notes concerns.

use serde::{Deserialize, Serialize};

use crate::model::TaskStatus;
use crate::severity::Severity;

/// One issue the reviewer raised about a task's diff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewFinding {
    /// `Severity::as_str()` form on the wire; parsed via [`ReviewFinding::severity`].
    pub severity: String,
    pub message: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
}

impl ReviewFinding {
    /// Parsed severity, defaulting to [`Severity::Medium`] when the
    /// reviewer emitted an unrecognised label (fail safe — unknown =
    /// blocking-ish).
    pub fn severity(&self) -> Severity {
        self.severity.parse().unwrap_or(Severity::Medium)
    }
}

/// The reviewer agent's structured output for one task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewReport {
    /// Top-line decision the agent made.
    pub verdict: Verdict,
    #[serde(default)]
    pub findings: Vec<ReviewFinding>,
    #[serde(default)]
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Diff is good; merge it.
    Approve,
    /// Diff needs changes; send the task back to the worker.
    Rework,
}

impl ReviewReport {
    /// Highest severity among findings, or `None` when clean.
    pub fn max_severity(&self) -> Option<Severity> {
        crate::severity::max_severity(&self.findings, ReviewFinding::severity)
    }

    /// Decide the task's next status from this report.
    ///
    /// - `Approve` with no finding at/above `block_gate` → [`TaskStatus::Done`]
    ///   (the orchestrator then merges the task branch).
    /// - Otherwise → [`TaskStatus::Todo`] (rework: the task re-enters the
    ///   queue with the reviewer's notes appended to its context).
    ///
    /// A reviewer that says `Approve` but left a blocking finding is
    /// overridden to rework — the structured findings win over the
    /// top-line verdict, because that's the safer disagreement to resolve
    /// automatically.
    pub fn next_status(&self, block_gate: Severity) -> TaskStatus {
        let has_blocker = self
            .max_severity()
            .is_some_and(|s| s.meets_or_exceeds(block_gate));
        match self.verdict {
            Verdict::Approve if !has_blocker => TaskStatus::Done,
            _ => TaskStatus::Todo,
        }
    }

    /// Notes to append to the task context on rework: the summary plus a
    /// bulleted list of blocking findings.
    pub fn rework_notes(&self, block_gate: Severity) -> String {
        let mut out = String::new();
        if !self.summary.is_empty() {
            out.push_str(&self.summary);
            out.push('\n');
        }
        for f in &self.findings {
            if f.severity().meets_or_exceeds(block_gate) {
                let loc = match (&f.file, f.line) {
                    (Some(file), Some(line)) => format!(" ({file}:{line})"),
                    (Some(file), None) => format!(" ({file})"),
                    _ => String::new(),
                };
                out.push_str(&format!("- [{}]{} {}\n", f.severity(), loc, f.message));
            }
        }
        out
    }
}

/// Parse a reviewer agent's JSON output into a [`ReviewReport`].
pub fn parse_review(json: &str) -> Result<ReviewReport, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid review report: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_approve_goes_to_done() {
        let r = ReviewReport {
            verdict: Verdict::Approve,
            findings: vec![],
            summary: "looks good".into(),
        };
        assert_eq!(r.next_status(Severity::Medium), TaskStatus::Done);
    }

    #[test]
    fn approve_with_low_finding_under_gate_still_done() {
        let r = ReviewReport {
            verdict: Verdict::Approve,
            findings: vec![ReviewFinding {
                severity: "low".into(),
                message: "nit: naming".into(),
                file: None,
                line: None,
            }],
            summary: String::new(),
        };
        assert_eq!(r.next_status(Severity::Medium), TaskStatus::Done);
    }

    #[test]
    fn approve_overridden_to_rework_by_blocking_finding() {
        // Agent said approve but flagged a high-severity bug — findings win.
        let r = ReviewReport {
            verdict: Verdict::Approve,
            findings: vec![ReviewFinding {
                severity: "high".into(),
                message: "off-by-one in loop".into(),
                file: Some("src/x.rs".into()),
                line: Some(10),
            }],
            summary: String::new(),
        };
        assert_eq!(r.next_status(Severity::Medium), TaskStatus::Todo);
    }

    #[test]
    fn explicit_rework_goes_to_todo() {
        let r = ReviewReport {
            verdict: Verdict::Rework,
            findings: vec![],
            summary: "needs tests".into(),
        };
        assert_eq!(r.next_status(Severity::High), TaskStatus::Todo);
    }

    #[test]
    fn rework_notes_lists_only_blockers_with_location() {
        let r = ReviewReport {
            verdict: Verdict::Rework,
            findings: vec![
                ReviewFinding {
                    severity: "low".into(),
                    message: "style".into(),
                    file: None,
                    line: None,
                },
                ReviewFinding {
                    severity: "high".into(),
                    message: "missing error handling".into(),
                    file: Some("src/y.rs".into()),
                    line: Some(42),
                },
            ],
            summary: "almost there".into(),
        };
        let notes = r.rework_notes(Severity::Medium);
        assert!(notes.contains("almost there"));
        assert!(notes.contains("missing error handling"));
        assert!(notes.contains("src/y.rs:42"));
        assert!(!notes.contains("style")); // below gate, excluded
    }

    #[test]
    fn parse_review_reads_agent_json() {
        let json = r#"{
            "verdict": "rework",
            "summary": "tighten this up",
            "findings": [
                {"severity": "medium", "message": "no bounds check", "file": "a.rs", "line": 7}
            ]
        }"#;
        let r = parse_review(json).unwrap();
        assert_eq!(r.verdict, Verdict::Rework);
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.max_severity(), Some(Severity::Medium));
    }

    #[test]
    fn unknown_severity_defaults_to_medium() {
        let f = ReviewFinding {
            severity: "spicy".into(),
            message: "x".into(),
            file: None,
            line: None,
        };
        assert_eq!(f.severity(), Severity::Medium);
    }

    #[test]
    fn parse_review_rejects_garbage() {
        assert!(parse_review("{").is_err());
    }
}
