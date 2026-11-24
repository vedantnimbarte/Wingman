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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReviewFinding {
    /// `Severity::as_str()` form on the wire; parsed via [`ReviewFinding::severity`].
    pub severity: String,
    pub message: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    /// `gap | bug | verification_miss | scope_drift | over_engineering`.
    /// Free text on the wire; only `over_engineering` changes behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    /// Stable id (`R1`, `R2`, …) assigned by [`ReviewReport::number_findings`]
    /// so the next round can say what happened to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl ReviewFinding {
    /// Parsed severity, defaulting to [`Severity::Medium`] when the
    /// reviewer emitted an unrecognised label (fail safe — unknown =
    /// blocking-ish).
    pub fn severity(&self) -> Severity {
        self.severity.parse().unwrap_or(Severity::Medium)
    }

    /// Over-engineering is reported, never a reason to send work back on its
    /// own: a reviewer that bounces "too clever" loops correct work.
    pub fn can_block(&self) -> bool {
        self.class.as_deref() != Some("over_engineering")
    }

    fn blocks(&self, gate: Severity) -> bool {
        self.can_block() && self.severity().meets_or_exceeds(gate)
    }

    fn line_item(&self) -> String {
        let loc = match (&self.file, self.line) {
            (Some(file), Some(line)) => format!(" ({file}:{line})"),
            (Some(file), None) => format!(" ({file})"),
            _ => String::new(),
        };
        let id = self
            .id
            .as_deref()
            .map(|i| format!("{i} "))
            .unwrap_or_default();
        format!("- {id}[{}]{loc} {}", self.severity(), self.message)
    }
}

/// What the reviewer found when it re-checked one earlier finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorStatus {
    Fixed,
    NotFixed,
    NoLongerApplies,
}

/// One earlier finding, re-checked against the new diff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriorCheck {
    pub id: String,
    pub status: PriorStatus,
    /// `file:line` for `fixed`; what is still wrong for `not_fixed`.
    #[serde(default)]
    pub evidence: String,
}

/// The reviewer agent's structured output for one task.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReviewReport {
    /// Top-line decision the agent made.
    pub verdict: Verdict,
    #[serde(default)]
    pub findings: Vec<ReviewFinding>,
    #[serde(default)]
    pub summary: String,
    /// What must be true for the change to be right, written before the
    /// reviewer reads the worker's own account.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hypotheses: Vec<String>,
    /// Earlier rounds' findings, re-checked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prior: Vec<PriorCheck>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Diff is good; merge it.
    #[default]
    Approve,
    /// Diff needs changes; send the task back to the worker.
    Rework,
}

impl ReviewReport {
    /// Give this round's findings ids that continue from the earlier
    /// rounds (`R1`, `R2`, …), so the next round can refer back to them.
    pub fn number_findings(&mut self, earlier: &[ReviewReport]) {
        let mut next = earlier
            .iter()
            .flat_map(|r| &r.findings)
            .filter_map(|f| f.id.as_deref()?.strip_prefix('R')?.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        for f in &mut self.findings {
            next += 1;
            f.id = Some(format!("R{next}"));
        }
    }

    /// Earlier findings this round says are `not_fixed`. An id the reviewer
    /// made up, or one it skipped, is ignored: an unchecked finding is not
    /// evidence of a defect, and failing open matches how a missing verdict
    /// is treated.
    fn still_open<'a>(&self, earlier: &'a [ReviewReport]) -> Vec<&'a ReviewFinding> {
        self.prior
            .iter()
            .filter(|c| c.status == PriorStatus::NotFixed)
            .filter_map(|c| {
                earlier
                    .iter()
                    .flat_map(|r| &r.findings)
                    .find(|f| f.id.as_deref() == Some(c.id.as_str()))
            })
            .collect()
    }

    /// Highest severity among findings, or `None` when clean.
    pub fn max_severity(&self) -> Option<Severity> {
        crate::severity::max_severity(&self.findings, ReviewFinding::severity)
    }

    /// Decide the task's next status from this report and the earlier
    /// rounds on the same task.
    ///
    /// Rework iff a finding at/above `block_gate` is new this round, or an
    /// earlier one is still `not_fixed`; otherwise Done. Over-engineering
    /// findings never block on their own. Structured findings win over the
    /// top-line verdict in *both* directions:
    ///
    /// - `Approve` with a blocking finding → rework (don't merge a flagged bug).
    /// - `Rework` with no blocking finding → Done. A meticulous model often
    ///   returns `rework` for a change that already passed its acceptance
    ///   checks without citing anything concrete; honoring that bare verdict
    ///   loops correct work until the retry ladder blocks the whole run. The
    ///   reviewer must name a real defect (a finding at/above the gate) to
    ///   send work back.
    pub fn next_status(&self, earlier: &[ReviewReport], block_gate: Severity) -> TaskStatus {
        let blocked = self.findings.iter().any(|f| f.blocks(block_gate))
            || self
                .still_open(earlier)
                .iter()
                .any(|f| f.blocks(block_gate));
        if blocked {
            TaskStatus::Todo
        } else {
            TaskStatus::Done
        }
    }

    /// Notes to append to the task context on rework: the summary, earlier
    /// findings still not fixed, then this round's blocking findings.
    pub fn rework_notes(&self, earlier: &[ReviewReport], block_gate: Severity) -> String {
        let mut out = String::new();
        if !self.summary.is_empty() {
            out.push_str(&self.summary);
            out.push('\n');
        }
        let open: Vec<_> = self
            .still_open(earlier)
            .into_iter()
            .filter(|f| f.blocks(block_gate))
            .collect();
        if !open.is_empty() {
            out.push_str("Still not fixed from the last review:\n");
            for f in open {
                out.push_str(&f.line_item());
                out.push('\n');
            }
        }
        for f in self.findings.iter().filter(|f| f.blocks(block_gate)) {
            out.push_str(&f.line_item());
            out.push('\n');
        }
        out
    }
}

/// What the inline reviewer hands back to finalize: rework notes when the
/// task goes back, and the numbered report to record either way. `report` is
/// `None` when the reviewer call or its parse failed.
#[derive(Debug, Clone, Default)]
pub struct ReviewerOutput {
    pub rework: Option<String>,
    pub report: Option<ReviewReport>,
    /// The QA gate's report, when `per_task_qa` is on. A QA bounce leaves
    /// `report` empty: the reviewer is skipped for that round.
    pub qa: Option<crate::qa::QaReport>,
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
            ..Default::default()
        };
        assert_eq!(r.next_status(&[], Severity::Medium), TaskStatus::Done);
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
                ..Default::default()
            }],
            summary: String::new(),
            ..Default::default()
        };
        assert_eq!(r.next_status(&[], Severity::Medium), TaskStatus::Done);
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
                ..Default::default()
            }],
            summary: String::new(),
            ..Default::default()
        };
        assert_eq!(r.next_status(&[], Severity::Medium), TaskStatus::Todo);
    }

    #[test]
    fn rework_verdict_without_a_blocking_finding_approves() {
        // A bare `rework` with no concrete finding must not loop work that
        // already passed acceptance — findings drive the decision, not the
        // verdict word.
        let r = ReviewReport {
            verdict: Verdict::Rework,
            findings: vec![],
            summary: "needs tests".into(),
            ..Default::default()
        };
        assert_eq!(r.next_status(&[], Severity::Medium), TaskStatus::Done);
    }

    #[test]
    fn rework_verdict_with_a_blocking_finding_reworks() {
        let r = ReviewReport {
            verdict: Verdict::Rework,
            findings: vec![ReviewFinding {
                severity: "high".into(),
                message: "panics on empty input".into(),
                file: None,
                line: None,
                ..Default::default()
            }],
            summary: String::new(),
            ..Default::default()
        };
        assert_eq!(r.next_status(&[], Severity::Medium), TaskStatus::Todo);
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
                    ..Default::default()
                },
                ReviewFinding {
                    severity: "high".into(),
                    message: "missing error handling".into(),
                    file: Some("src/y.rs".into()),
                    line: Some(42),
                    ..Default::default()
                },
            ],
            summary: "almost there".into(),
            ..Default::default()
        };
        let notes = r.rework_notes(&[], Severity::Medium);
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
            ..Default::default()
        };
        assert_eq!(f.severity(), Severity::Medium);
    }

    #[test]
    fn parse_review_rejects_garbage() {
        assert!(parse_review("{").is_err());
    }

    fn finding(severity: &str, class: Option<&str>) -> ReviewFinding {
        ReviewFinding {
            severity: severity.into(),
            message: "m".into(),
            class: class.map(str::to_string),
            ..Default::default()
        }
    }

    fn round1() -> ReviewReport {
        let mut r = ReviewReport {
            findings: vec![finding("high", Some("bug")), finding("low", None)],
            ..Default::default()
        };
        r.number_findings(&[]);
        r
    }

    #[test]
    fn over_engineering_alone_never_reworks() {
        let r = ReviewReport {
            findings: vec![finding("critical", Some("over_engineering"))],
            ..Default::default()
        };
        assert_eq!(r.next_status(&[], Severity::High), TaskStatus::Done);
        assert!(!r.rework_notes(&[], Severity::High).contains("[critical]"));
    }

    #[test]
    fn ids_continue_across_rounds() {
        let earlier = [round1()];
        assert_eq!(earlier[0].findings[1].id.as_deref(), Some("R2"));
        let mut r2 = ReviewReport {
            findings: vec![finding("low", None)],
            ..Default::default()
        };
        r2.number_findings(&earlier);
        assert_eq!(r2.findings[0].id.as_deref(), Some("R3"));
    }

    #[test]
    fn earlier_blocker_still_not_fixed_reworks() {
        let earlier = [round1()];
        let r2 = ReviewReport {
            prior: vec![PriorCheck {
                id: "R1".into(),
                status: PriorStatus::NotFixed,
                evidence: "still panics".into(),
            }],
            ..Default::default()
        };
        assert_eq!(r2.next_status(&earlier, Severity::High), TaskStatus::Todo);
        let notes = r2.rework_notes(&earlier, Severity::High);
        assert!(notes.contains("Still not fixed"), "{notes}");
        assert!(notes.contains("R1 [high]"), "{notes}");
    }

    #[test]
    fn fixed_unknown_or_below_gate_priors_do_not_rework() {
        let earlier = [round1()];
        let check = |id: &str, status| PriorCheck {
            id: id.into(),
            status,
            evidence: String::new(),
        };
        let r2 = ReviewReport {
            prior: vec![
                check("R1", PriorStatus::Fixed),
                check("R2", PriorStatus::NotFixed), // low: under the gate
                check("R99", PriorStatus::NotFixed), // made up
            ],
            ..Default::default()
        };
        assert_eq!(r2.next_status(&earlier, Severity::High), TaskStatus::Done);
    }

    #[test]
    fn parse_reads_new_fields_and_old_json_still_parses() {
        let r = parse_review(
            r#"{"hypotheses":["h"],"verdict":"approve","prior":[{"id":"R1","status":"no_longer_applies"}],
                "findings":[{"severity":"low","message":"x","class":"over_engineering"}]}"#,
        )
        .unwrap();
        assert_eq!(r.prior[0].status, PriorStatus::NoLongerApplies);
        assert!(!r.findings[0].can_block());
        assert!(parse_review(r#"{"verdict":"approve"}"#)
            .unwrap()
            .prior
            .is_empty());
    }
}
