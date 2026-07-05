//! E8 — PR-side automation: the auto-merge gate + richer PR-body sections.
//!
//! [`crate::pr`] already opens the PR and renders the base body. E8 adds
//! the *decision* of whether to auto-merge it, and the extra body sections
//! that make a human review a rubber stamp. The gate is the keystone of
//! the copilot tier: it's the single place where every upstream signal
//! converges — E1's approval tier, CI status, E7's per-task review
//! findings, R6's security pass, and J10's critic veto.
//!
//! Auto-merge fires only when *every* condition holds. Each failing
//! condition contributes a human-readable reason, so a held PR explains
//! itself in the notification rather than silently waiting.

use crate::model::{Acceptance, RunState};
use crate::severity::Severity;

/// All signals the auto-merge gate weighs. Pre-evaluated by the caller so
/// the decision itself is pure and exhaustively testable.
#[derive(Debug, Clone)]
pub struct AutoMergeInputs {
    /// `[pilot.pr].auto_merge` master switch.
    pub config_auto_merge: bool,
    /// E1 classified the plan as `auto` (low-risk). Auto-merge is only for
    /// runs that were trusted from the start.
    pub tier_was_auto: bool,
    /// CI status: `Some(true)` green, `Some(false)` red, `None` unknown.
    pub ci_green: Option<bool>,
    /// `[pilot.pr].require_ci_green`.
    pub require_ci_green: bool,
    /// Highest severity among E7 per-task review findings, if any.
    pub review_max_severity: Option<Severity>,
    /// R6 security pass blocked (any finding at/above its gate).
    pub security_blocks: bool,
    /// J10 critic raised a high+ risk → hard veto.
    pub critic_vetoes: bool,
    /// Any write touched a `dangerous_paths` glob.
    pub dangerous_paths_touched: bool,
    /// Parsed `[pilot.pr].auto_merge_max_severity` — review findings
    /// strictly above this block auto-merge.
    pub merge_max_severity: Severity,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AutoMergeDecision {
    /// Every gate passed — safe to merge automatically.
    Merge,
    /// At least one gate failed; each reason is human-readable.
    Hold { reasons: Vec<String> },
}

impl AutoMergeDecision {
    pub fn is_merge(&self) -> bool {
        matches!(self, Self::Merge)
    }
}

/// Decide whether to auto-merge. Returns [`AutoMergeDecision::Merge`] only
/// when all conditions pass.
pub fn decide_auto_merge(inputs: &AutoMergeInputs) -> AutoMergeDecision {
    let mut reasons = Vec::new();

    if !inputs.config_auto_merge {
        reasons.push("auto-merge is disabled in [pilot.pr]".to_string());
    }
    if !inputs.tier_was_auto {
        reasons.push("plan was not auto-approved by the trust gate (E1)".to_string());
    }
    if inputs.require_ci_green {
        match inputs.ci_green {
            Some(true) => {}
            Some(false) => reasons.push("CI is red".to_string()),
            None => reasons.push("CI status is unknown".to_string()),
        }
    }
    if inputs.dangerous_paths_touched {
        reasons.push("plan touches dangerous_paths".to_string());
    }
    if let Some(sev) = inputs.review_max_severity {
        if sev > inputs.merge_max_severity {
            reasons.push(format!(
                "review finding severity {} exceeds auto_merge_max_severity {}",
                sev, inputs.merge_max_severity
            ));
        }
    }
    if inputs.security_blocks {
        reasons.push("security pass (R6) flagged a blocking finding".to_string());
    }
    if inputs.critic_vetoes {
        reasons.push("critic (J10) vetoed the merge".to_string());
    }

    if reasons.is_empty() {
        AutoMergeDecision::Merge
    } else {
        AutoMergeDecision::Hold { reasons }
    }
}

// ---------------------------------------------------------------------------
// Richer PR-body sections (E8 §2)
// ---------------------------------------------------------------------------

/// Render the "Test plan" section: every task's acceptance check,
/// pre-checked (they passed before the task reached review).
pub fn render_test_plan(state: &RunState) -> String {
    let mut out = String::from("## Test plan\n\n");
    let mut any = false;
    for t in &state.tasks {
        for a in &t.acceptance {
            any = true;
            let line = match a {
                Acceptance::Shell { cmd } => format!("- [x] `{cmd}`"),
                Acceptance::Grep { pattern, path } => {
                    format!("- [x] grep `{pattern}` in `{path}`")
                }
                Acceptance::Http { url, .. } => format!("- [x] GET `{url}`"),
                Acceptance::Run { target, .. } => format!("- [x] run `{target}`"),
                Acceptance::Assert { screenshot, .. } => {
                    format!("- [x] assert rendered `{screenshot}`")
                }
            };
            out.push_str(&line);
            out.push('\n');
        }
    }
    if !any {
        out.push_str("_No executable acceptance checks were declared._\n");
    }
    out
}

/// Render the "What to scrutinize" section: files touching dangerous
/// paths (E1) and any task flagged by `flagged_task_ids` (e.g. tasks that
/// took more than one retry rung). Empty when there's nothing notable.
pub fn render_scrutiny(
    state: &RunState,
    dangerous_files: &[String],
    flagged_task_ids: &[String],
) -> String {
    if dangerous_files.is_empty() && flagged_task_ids.is_empty() {
        return String::new();
    }
    let mut out = String::from("## What to scrutinize\n\n");
    for f in dangerous_files {
        out.push_str(&format!("- ⚠️ touches dangerous path: `{f}`\n"));
    }
    for id in flagged_task_ids {
        let title = state
            .task(id)
            .map(|t| t.title.as_str())
            .unwrap_or("(unknown task)");
        out.push_str(&format!("- 🔁 task #{id} ({title}) needed >1 retry\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Acceptance, Role, Task};

    fn passing_inputs() -> AutoMergeInputs {
        AutoMergeInputs {
            config_auto_merge: true,
            tier_was_auto: true,
            ci_green: Some(true),
            require_ci_green: true,
            review_max_severity: Some(Severity::Low),
            security_blocks: false,
            critic_vetoes: false,
            dangerous_paths_touched: false,
            merge_max_severity: Severity::Low,
        }
    }

    #[test]
    fn all_green_merges() {
        assert_eq!(
            decide_auto_merge(&passing_inputs()),
            AutoMergeDecision::Merge
        );
    }

    #[test]
    fn disabled_config_holds() {
        let i = AutoMergeInputs {
            config_auto_merge: false,
            ..passing_inputs()
        };
        let d = decide_auto_merge(&i);
        assert!(!d.is_merge());
        if let AutoMergeDecision::Hold { reasons } = d {
            assert!(reasons.iter().any(|r| r.contains("disabled")));
        }
    }

    #[test]
    fn non_auto_tier_holds() {
        let i = AutoMergeInputs {
            tier_was_auto: false,
            ..passing_inputs()
        };
        assert!(!decide_auto_merge(&i).is_merge());
    }

    #[test]
    fn red_ci_holds() {
        let i = AutoMergeInputs {
            ci_green: Some(false),
            ..passing_inputs()
        };
        assert!(!decide_auto_merge(&i).is_merge());
    }

    #[test]
    fn unknown_ci_holds_when_required() {
        let i = AutoMergeInputs {
            ci_green: None,
            ..passing_inputs()
        };
        assert!(!decide_auto_merge(&i).is_merge());
    }

    #[test]
    fn unknown_ci_ok_when_not_required() {
        let i = AutoMergeInputs {
            ci_green: None,
            require_ci_green: false,
            ..passing_inputs()
        };
        assert!(decide_auto_merge(&i).is_merge());
    }

    #[test]
    fn dangerous_paths_hold() {
        let i = AutoMergeInputs {
            dangerous_paths_touched: true,
            ..passing_inputs()
        };
        assert!(!decide_auto_merge(&i).is_merge());
    }

    #[test]
    fn review_finding_above_gate_holds() {
        let i = AutoMergeInputs {
            review_max_severity: Some(Severity::High),
            merge_max_severity: Severity::Low,
            ..passing_inputs()
        };
        assert!(!decide_auto_merge(&i).is_merge());
    }

    #[test]
    fn review_finding_at_gate_ok() {
        let i = AutoMergeInputs {
            review_max_severity: Some(Severity::Medium),
            merge_max_severity: Severity::Medium,
            ..passing_inputs()
        };
        assert!(decide_auto_merge(&i).is_merge());
    }

    #[test]
    fn security_block_holds() {
        let i = AutoMergeInputs {
            security_blocks: true,
            ..passing_inputs()
        };
        assert!(!decide_auto_merge(&i).is_merge());
    }

    #[test]
    fn critic_veto_holds() {
        let i = AutoMergeInputs {
            critic_vetoes: true,
            ..passing_inputs()
        };
        assert!(!decide_auto_merge(&i).is_merge());
    }

    #[test]
    fn multiple_failures_accumulate_reasons() {
        let i = AutoMergeInputs {
            config_auto_merge: false,
            ci_green: Some(false),
            critic_vetoes: true,
            ..passing_inputs()
        };
        if let AutoMergeDecision::Hold { reasons } = decide_auto_merge(&i) {
            assert_eq!(reasons.len(), 3);
        } else {
            panic!("expected hold");
        }
    }

    fn state_with_acceptance() -> RunState {
        let mut s = RunState::new("r1", "g", "abc", "b");
        let mut t = Task::new("t1", Role::Developer, "add flag");
        t.acceptance = vec![
            Acceptance::Shell {
                cmd: "cargo check".into(),
            },
            Acceptance::Grep {
                pattern: "version-only".into(),
                path: "args.rs".into(),
            },
        ];
        s.tasks.push(t);
        s
    }

    #[test]
    fn test_plan_lists_acceptance_checks() {
        let s = state_with_acceptance();
        let tp = render_test_plan(&s);
        assert!(tp.contains("## Test plan"));
        assert!(tp.contains("- [x] `cargo check`"));
        assert!(tp.contains("grep `version-only`"));
    }

    #[test]
    fn test_plan_handles_no_checks() {
        let s = RunState::new("r1", "g", "abc", "b");
        assert!(render_test_plan(&s).contains("No executable acceptance"));
    }

    #[test]
    fn scrutiny_empty_when_nothing_notable() {
        let s = RunState::new("r1", "g", "abc", "b");
        assert!(render_scrutiny(&s, &[], &[]).is_empty());
    }

    #[test]
    fn scrutiny_flags_dangerous_and_retried() {
        let mut s = RunState::new("r1", "g", "abc", "b");
        s.tasks
            .push(Task::new("t1", Role::Developer, "auth change"));
        let md = render_scrutiny(
            &s,
            &["crates/auth/src/login.rs".to_string()],
            &["t1".to_string()],
        );
        assert!(md.contains("dangerous path"));
        assert!(md.contains("crates/auth/src/login.rs"));
        assert!(md.contains("task #t1"));
        assert!(md.contains("auth change"));
    }
}
