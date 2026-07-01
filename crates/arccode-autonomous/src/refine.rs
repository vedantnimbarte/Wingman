//! J1 — goal refinement & negotiation (before planning).
//!
//! Today the planner accepts the goal as-is. This stage runs *before* E2:
//! a refinement agent reads the goal + scans the repo and may
//!
//! - ask up to `max_clarifying_questions` (only when the answer materially
//!   changes the plan),
//! - restate an ambiguous-but-inferable goal ("I think you mean X"),
//! - challenge the goal ("there's already a `--quiet` flag"),
//! - suggest up to two better approaches with one-line tradeoffs.
//!
//! This module parses that agent's structured output and decides what to
//! do with it, honoring `[pilot.refine]`. The auto tier accepts a
//! high-confidence restatement silently; medium confidence opens a
//! notify-only window; low confidence (or any blocking challenge / pending
//! question) escalates to the user.

use serde::{Deserialize, Serialize};

use arccode_config::PilotRefineConfig;

use crate::severity::Severity;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    fn parse(s: &str) -> Confidence {
        match s.trim().to_ascii_lowercase().as_str() {
            "high" => Confidence::High,
            "medium" | "med" => Confidence::Medium,
            _ => Confidence::Low,
        }
    }
}

/// A reason the agent thinks the goal is wrong or could be better.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Challenge {
    pub severity: String,
    pub message: String,
}

impl Challenge {
    pub fn severity(&self) -> Severity {
        self.severity.parse().unwrap_or(Severity::Medium)
    }
}

/// An alternative approach the agent proposes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alternative {
    pub description: String,
    #[serde(default)]
    pub tradeoff: String,
}

/// The refinement agent's structured output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefinementReport {
    #[serde(default)]
    pub clarifying_questions: Vec<String>,
    #[serde(default)]
    pub goal_restatement: Option<String>,
    /// "low" | "medium" | "high" — confidence in the restatement.
    #[serde(default)]
    pub restatement_confidence: Option<String>,
    #[serde(default)]
    pub challenges: Vec<Challenge>,
    #[serde(default)]
    pub alternatives: Vec<Alternative>,
}

/// What the orchestrator should do after refinement.
#[derive(Debug, Clone, PartialEq)]
pub enum RefineAction {
    /// Proceed straight to planning with this (possibly restated) goal.
    Proceed { goal: String },
    /// Proceed with this goal unless the user vetoes within the window.
    NotifyWindow { goal: String, note: String },
    /// Stop and ask the user: pending questions and/or blocking challenges.
    AskUser {
        questions: Vec<String>,
        challenges: Vec<String>,
        alternatives: Vec<Alternative>,
    },
}

fn challenge_threshold(config: &PilotRefineConfig) -> Option<Severity> {
    match config
        .challenge_threshold
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "off" | "none" | "" => None,
        other => other.parse::<Severity>().ok().or(Some(Severity::Medium)),
    }
}

/// Decide what to do with a refinement report.
pub fn decide(
    report: &RefinementReport,
    config: &PilotRefineConfig,
    original_goal: &str,
) -> RefineAction {
    // 1. Pending clarifying questions (capped) always escalate.
    let questions: Vec<String> = report
        .clarifying_questions
        .iter()
        .take(config.max_clarifying_questions as usize)
        .cloned()
        .collect();

    // 2. Challenges at or above the configured threshold escalate.
    let blocking: Vec<String> = match challenge_threshold(config) {
        None => Vec::new(),
        Some(gate) => report
            .challenges
            .iter()
            .filter(|c| c.severity().meets_or_exceeds(gate))
            .map(|c| format!("[{}] {}", c.severity(), c.message))
            .collect(),
    };

    if !questions.is_empty() || !blocking.is_empty() {
        let alternatives = if config.suggest_alternatives {
            report.alternatives.clone()
        } else {
            Vec::new()
        };
        return RefineAction::AskUser {
            questions,
            challenges: blocking,
            alternatives,
        };
    }

    // 3. Restatement handling by confidence.
    if let Some(restated) = &report.goal_restatement {
        let conf = report
            .restatement_confidence
            .as_deref()
            .map(Confidence::parse)
            .unwrap_or(Confidence::Low);
        return match conf {
            Confidence::High => RefineAction::Proceed {
                goal: restated.clone(),
            },
            Confidence::Medium => RefineAction::NotifyWindow {
                goal: restated.clone(),
                note: format!("interpreting goal as: {restated}"),
            },
            Confidence::Low => RefineAction::AskUser {
                questions: vec![format!("Did you mean: {restated}?")],
                challenges: Vec::new(),
                alternatives: if config.suggest_alternatives {
                    report.alternatives.clone()
                } else {
                    Vec::new()
                },
            },
        };
    }

    // 4. Nothing to negotiate — proceed with the original goal.
    RefineAction::Proceed {
        goal: original_goal.to_string(),
    }
}

/// Parse the refinement agent's JSON output.
pub fn parse_refinement(json: &str) -> Result<RefinementReport, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid refinement report: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PilotRefineConfig {
        PilotRefineConfig::default() // max 3 questions, threshold medium, suggest alternatives
    }

    fn empty() -> RefinementReport {
        RefinementReport {
            clarifying_questions: vec![],
            goal_restatement: None,
            restatement_confidence: None,
            challenges: vec![],
            alternatives: vec![],
        }
    }

    #[test]
    fn clean_report_proceeds_with_original() {
        let action = decide(&empty(), &cfg(), "add a flag");
        assert_eq!(
            action,
            RefineAction::Proceed {
                goal: "add a flag".into()
            }
        );
    }

    #[test]
    fn pending_questions_escalate() {
        let r = RefinementReport {
            clarifying_questions: vec!["which config file?".into()],
            ..empty()
        };
        match decide(&r, &cfg(), "g") {
            RefineAction::AskUser { questions, .. } => assert_eq!(questions.len(), 1),
            other => panic!("expected AskUser, got {other:?}"),
        }
    }

    #[test]
    fn questions_capped_at_config_max() {
        let r = RefinementReport {
            clarifying_questions: vec![
                "q1".into(),
                "q2".into(),
                "q3".into(),
                "q4".into(),
                "q5".into(),
            ],
            ..empty()
        };
        match decide(&r, &cfg(), "g") {
            RefineAction::AskUser { questions, .. } => assert_eq!(questions.len(), 3),
            other => panic!("expected AskUser, got {other:?}"),
        }
    }

    #[test]
    fn high_severity_challenge_escalates() {
        let r = RefinementReport {
            challenges: vec![Challenge {
                severity: "high".into(),
                message: "conflicts with auth-v2".into(),
            }],
            ..empty()
        };
        assert!(matches!(
            decide(&r, &cfg(), "g"),
            RefineAction::AskUser { .. }
        ));
    }

    #[test]
    fn low_challenge_under_threshold_does_not_escalate() {
        let r = RefinementReport {
            challenges: vec![Challenge {
                severity: "low".into(),
                message: "minor".into(),
            }],
            ..empty()
        };
        assert_eq!(
            decide(&r, &cfg(), "g"),
            RefineAction::Proceed { goal: "g".into() }
        );
    }

    #[test]
    fn challenge_threshold_off_ignores_all_challenges() {
        let mut c = cfg();
        c.challenge_threshold = "off".into();
        let r = RefinementReport {
            challenges: vec![Challenge {
                severity: "critical".into(),
                message: "x".into(),
            }],
            ..empty()
        };
        assert_eq!(
            decide(&r, &c, "g"),
            RefineAction::Proceed { goal: "g".into() }
        );
    }

    #[test]
    fn high_confidence_restatement_proceeds_silently() {
        let r = RefinementReport {
            goal_restatement: Some("extend the existing --quiet flag".into()),
            restatement_confidence: Some("high".into()),
            ..empty()
        };
        assert_eq!(
            decide(&r, &cfg(), "g"),
            RefineAction::Proceed {
                goal: "extend the existing --quiet flag".into()
            }
        );
    }

    #[test]
    fn medium_confidence_restatement_opens_window() {
        let r = RefinementReport {
            goal_restatement: Some("X".into()),
            restatement_confidence: Some("medium".into()),
            ..empty()
        };
        match decide(&r, &cfg(), "g") {
            RefineAction::NotifyWindow { goal, .. } => assert_eq!(goal, "X"),
            other => panic!("expected NotifyWindow, got {other:?}"),
        }
    }

    #[test]
    fn low_confidence_restatement_asks() {
        let r = RefinementReport {
            goal_restatement: Some("X".into()),
            restatement_confidence: Some("low".into()),
            ..empty()
        };
        assert!(matches!(
            decide(&r, &cfg(), "g"),
            RefineAction::AskUser { .. }
        ));
    }

    #[test]
    fn alternatives_suppressed_when_disabled() {
        let mut c = cfg();
        c.suggest_alternatives = false;
        let r = RefinementReport {
            clarifying_questions: vec!["q".into()],
            alternatives: vec![Alternative {
                description: "alt".into(),
                tradeoff: "faster".into(),
            }],
            ..empty()
        };
        match decide(&r, &c, "g") {
            RefineAction::AskUser { alternatives, .. } => assert!(alternatives.is_empty()),
            other => panic!("expected AskUser, got {other:?}"),
        }
    }

    #[test]
    fn parse_refinement_reads_json() {
        let json = r#"{
            "goal_restatement": "add --version-only to the CLI",
            "restatement_confidence": "high",
            "challenges": [{"severity": "low", "message": "minor overlap with --quiet"}],
            "alternatives": [{"description": "extend --quiet", "tradeoff": "less code"}]
        }"#;
        let r = parse_refinement(json).unwrap();
        assert_eq!(r.restatement_confidence.as_deref(), Some("high"));
        assert_eq!(r.alternatives.len(), 1);
    }
}
