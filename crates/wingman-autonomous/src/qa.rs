//! Per-task QA gate: is each acceptance criterion asserted by a test, at the
//! layer where its defect would live?
//!
//! Acceptance checks already *run* before a task reaches review (E3). What
//! they can't say is whether the change left a test behind that would catch
//! the regression next time. QA answers that with one model call over the
//! task's diff, before the reviewer: a ledger (criterion → test and layer)
//! and a list of gaps. A gap at/above `[pilot.pr].qa_rework_severity` sends
//! the task back and the reviewer is skipped for that round.

use serde::{Deserialize, Serialize};

use crate::severity::Severity;

/// One acceptance criterion and the test that asserts it, if any.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QaRow {
    pub criterion: String,
    /// `unit | integration | e2e | none`.
    #[serde(default)]
    pub layer: String,
    /// `file:line` of the asserting test.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// A behaviour the task needs that no test covers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QaGap {
    /// `low | medium | high | critical`; unknown parses as medium.
    pub severity: String,
    pub message: String,
    /// Layer the missing test belongs at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

impl QaGap {
    pub fn severity(&self) -> Severity {
        self.severity.parse().unwrap_or(Severity::Medium)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QaReport {
    #[serde(default)]
    pub ledger: Vec<QaRow>,
    #[serde(default)]
    pub gaps: Vec<QaGap>,
    #[serde(default)]
    pub summary: String,
}

impl QaReport {
    /// Rework iff a gap is at/above `gate`. A ledger row marked `none` is not
    /// a gap by itself: a docs or config change asserts nothing, and the model
    /// is the one that judges whether a missing test matters.
    pub fn blocks(&self, gate: Severity) -> bool {
        self.gaps
            .iter()
            .any(|g| g.severity().meets_or_exceeds(gate))
    }

    pub fn rework_notes(&self, gate: Severity) -> String {
        let mut out = String::from("QA found untested behaviour:\n");
        for g in self
            .gaps
            .iter()
            .filter(|g| g.severity().meets_or_exceeds(gate))
        {
            let layer = g
                .layer
                .as_deref()
                .map(|l| format!(" (add a {l} test)"))
                .unwrap_or_default();
            out.push_str(&format!("- [{}] {}{layer}\n", g.severity(), g.message));
        }
        out
    }
}

pub fn parse_qa(json: &str) -> Result<QaReport, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid QA report: {e}"))
}

pub const QA_SYSTEM: &str = "You are a QA engineer checking whether a change left tests \
    behind that would catch its own regression. For each acceptance criterion and each \
    behaviour the goal requires, find the test in the diff that asserts it and the layer \
    it sits at. Work bottom up: a defect in a function belongs in a unit test, one in how \
    parts fit together in an integration test, and only a user-visible flow in an \
    end-to-end test. Reply with ONLY a JSON object: \
    {\"ledger\":[{\"criterion\":\"...\",\"layer\":\"unit|integration|e2e|none\", \
    \"evidence\":\"file:line\"}], \
    \"gaps\":[{\"severity\":\"low|medium|high|critical\",\"message\":\"what is untested\", \
    \"layer\":\"unit|integration|e2e\"}], \"summary\":\"...\"}. \
    A gap is high only when the goal requires a behaviour, nothing in the diff tests it, \
    and a regression there would plausibly ship. Do not ask for tests of code the diff \
    does not touch. A change that needs no tests (docs, config, comments) has no gaps.";

/// The QA call's user message: the task, its criteria, then the diff.
pub fn qa_prompt(task: &crate::model::Task, diff: &str) -> String {
    let mut out = format!(
        "Task #{}: {}\nRole: {}\nGoal: {}\n",
        task.id,
        task.title,
        task.role.as_str(),
        task.goal
    );
    if !task.acceptance.is_empty() {
        out.push_str("\n## Acceptance criteria (all passed when run)\n");
        for a in &task.acceptance {
            out.push_str(&format!(
                "- {}\n",
                serde_json::to_string(a).unwrap_or_default()
            ));
        }
    }
    out.push_str(&format!("\n## Diff\n```diff\n{diff}\n```\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_gaps_at_or_above_the_gate_block() {
        let r = parse_qa(
            r#"{"ledger":[{"criterion":"parses empty input","layer":"none"}],
                "gaps":[{"severity":"medium","message":"no test for empty input","layer":"unit"}]}"#,
        )
        .unwrap();
        assert!(!r.blocks(Severity::High));
        assert!(r.blocks(Severity::Medium));
        let notes = r.rework_notes(Severity::Medium);
        assert!(
            notes.contains("[medium] no test for empty input (add a unit test)"),
            "{notes}"
        );
    }

    #[test]
    fn a_none_ledger_row_alone_does_not_block() {
        let r = parse_qa(r#"{"ledger":[{"criterion":"docs","layer":"none"}]}"#).unwrap();
        assert!(!r.blocks(Severity::Low));
    }

    #[test]
    fn unknown_severity_is_medium_and_garbage_is_an_error() {
        let g = QaGap {
            severity: "spicy".into(),
            ..Default::default()
        };
        assert_eq!(g.severity(), Severity::Medium);
        assert!(parse_qa("{").is_err());
    }
}
