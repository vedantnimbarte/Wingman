//! J10 — critic agent (always-on red team).
//!
//! A second model — ideally a *different family* than the primary, so its
//! errors are uncorrelated — runs alongside the planner and reviewer with
//! one job: disagree productively. It hits three points in the lifecycle:
//!
//! 1. **After planning** — "what would break this plan?" Risks above a
//!    threshold become guardrail tasks appended to the plan.
//! 2. **After each task review** — independent re-review focused on what
//!    the primary reviewer most often misses (security, perf, data loss).
//! 3. **Before auto-merge** — a final pass; any *high*-severity finding
//!    vetoes the merge regardless of E8's configured gate.
//!
//! This module is the decision core: parse the critic's structured output
//! and apply the veto / guardrail rules. The orchestrator owns picking the
//! critic's model and spawning it.

use serde::{Deserialize, Serialize};

use crate::severity::{max_severity, Severity};

/// The critic's hard veto threshold for auto-merge. Independent of (and
/// stricter than) E8's `auto_merge_max_severity` — the critic's whole
/// point is to override the primary path's risk tolerance.
pub const VETO_THRESHOLD: Severity = Severity::High;

/// Risks at or above this become guardrail tasks appended to the plan.
pub const GUARDRAIL_THRESHOLD: Severity = Severity::Medium;

/// One risk the critic raised.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Risk {
    pub severity: String,
    /// What could break.
    pub description: String,
    /// Optional concrete mitigation — becomes a guardrail task's goal.
    #[serde(default)]
    pub mitigation: Option<String>,
}

impl Risk {
    pub fn severity(&self) -> Severity {
        self.severity.parse().unwrap_or(Severity::Medium)
    }
}

/// The critic agent's structured output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CriticReport {
    #[serde(default)]
    pub risks: Vec<Risk>,
    #[serde(default)]
    pub summary: String,
}

/// A guardrail task the critic recommends inserting into the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct GuardrailTask {
    pub title: String,
    pub goal: String,
    pub severity: Severity,
}

impl CriticReport {
    pub fn max_severity(&self) -> Option<Severity> {
        max_severity(&self.risks, Risk::severity)
    }

    /// True when the critic should veto auto-merge: any risk at or above
    /// [`VETO_THRESHOLD`].
    pub fn vetoes_auto_merge(&self) -> bool {
        self.risks.iter().any(|r| r.severity() >= VETO_THRESHOLD)
    }

    /// Risks at or above [`GUARDRAIL_THRESHOLD`], converted into tasks the
    /// planner appends. A risk without a mitigation still produces a task
    /// ("investigate and address: …") so it isn't silently dropped.
    pub fn guardrail_tasks(&self) -> Vec<GuardrailTask> {
        self.risks
            .iter()
            .filter(|r| r.severity() >= GUARDRAIL_THRESHOLD)
            .map(|r| {
                let goal = r
                    .mitigation
                    .clone()
                    .unwrap_or_else(|| format!("Investigate and address: {}", r.description));
                GuardrailTask {
                    title: format!("[guardrail] {}", truncate(&r.description, 60)),
                    goal,
                    severity: r.severity(),
                }
            })
            .collect()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// Parse the critic agent's JSON output.
pub fn parse_critic(json: &str) -> Result<CriticReport, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid critic report: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn risk(sev: &str, desc: &str, mit: Option<&str>) -> Risk {
        Risk {
            severity: sev.into(),
            description: desc.into(),
            mitigation: mit.map(String::from),
        }
    }

    #[test]
    fn high_risk_vetoes_auto_merge() {
        let r = CriticReport {
            risks: vec![risk("high", "drops the users table without a backup", None)],
            summary: String::new(),
        };
        assert!(r.vetoes_auto_merge());
    }

    #[test]
    fn medium_risk_does_not_veto() {
        let r = CriticReport {
            risks: vec![risk("medium", "could be slow on large inputs", None)],
            summary: String::new(),
        };
        assert!(!r.vetoes_auto_merge());
    }

    #[test]
    fn critical_risk_vetoes() {
        let r = CriticReport {
            risks: vec![risk("critical", "RCE in the new endpoint", None)],
            summary: String::new(),
        };
        assert!(r.vetoes_auto_merge());
    }

    #[test]
    fn clean_report_does_not_veto() {
        let r = CriticReport {
            risks: vec![],
            summary: "looks robust".into(),
        };
        assert!(!r.vetoes_auto_merge());
        assert!(r.guardrail_tasks().is_empty());
    }

    #[test]
    fn guardrail_tasks_include_medium_and_above() {
        let r = CriticReport {
            risks: vec![
                risk("low", "minor style", None),
                risk("medium", "no rollback path", Some("add a down-migration")),
                risk("high", "no auth check", None),
            ],
            summary: String::new(),
        };
        let tasks = r.guardrail_tasks();
        assert_eq!(tasks.len(), 2); // medium + high, not low
        assert_eq!(tasks[0].goal, "add a down-migration");
        // High risk without mitigation gets a synthesised goal.
        assert!(tasks[1].goal.starts_with("Investigate and address:"));
    }

    #[test]
    fn guardrail_title_is_truncated() {
        let long = "a".repeat(200);
        let r = CriticReport {
            risks: vec![risk("high", &long, None)],
            summary: String::new(),
        };
        let tasks = r.guardrail_tasks();
        assert!(tasks[0].title.chars().count() <= "[guardrail] ".len() + 60);
    }

    #[test]
    fn unknown_severity_defaults_to_medium() {
        assert_eq!(risk("weird", "x", None).severity(), Severity::Medium);
    }

    #[test]
    fn parse_critic_reads_json() {
        let json = r#"{
            "summary": "two concerns",
            "risks": [
                {"severity": "high", "description": "no input validation", "mitigation": "validate at the boundary"},
                {"severity": "low", "description": "naming nit"}
            ]
        }"#;
        let r = parse_critic(json).unwrap();
        assert_eq!(r.risks.len(), 2);
        assert!(r.vetoes_auto_merge());
        assert_eq!(r.max_severity(), Some(Severity::High));
        assert_eq!(r.guardrail_tasks().len(), 1);
    }

    #[test]
    fn parse_critic_rejects_garbage() {
        assert!(parse_critic("nope").is_err());
    }
}
