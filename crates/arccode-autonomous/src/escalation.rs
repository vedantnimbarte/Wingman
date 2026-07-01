//! J15 hard escalation triggers + R1 reversibility enforcement.
//!
//! These are the *non-negotiable* lines a pilot run must not cross
//! without explicit human approval, regardless of tier or `--yes`. Per
//! plan.md § J15:
//!
//! - Net negative test count after a task.
//! - Any change to `dangerous_paths` (E1) without explicit goal mentioning it.
//! - Detected secrets in a diff (regex + entropy check).
//! - Cumulative spend ≥ `max_usd` × 0.8 (warn) or ≥ × 1.0 (halt).
//! - 3 consecutive failed runs on related goals — likely the agent is
//!   stuck in a wrong mental model and needs human reset.
//! - License / copyright headers being modified.
//! - Force-push to any non-`arccode/auto/*` branch.
//!
//! Plus R1 reversibility:
//!
//! - assist tier: surface classification in approval prompt; no blocking.
//! - copilot tier: `hard` requires hard-gate approval regardless of E1
//!   trust score; `irreversible` always escalates.
//! - autopilot tier: `hard` requires notify-only window even when trust
//!   score is high; `irreversible` always escalates, never auto-approved
//!   or auto-merged.

use crate::approval::ApprovalTier;
use crate::model::{Reversibility, RunState, Task, TaskStatus};
use crate::planner::PlannedTask;
use arccode_config::PilotTier;

/// One non-negotiable trigger. Each kind carries enough context for the
/// R3 escalation packet to surface the issue to the user.
#[derive(Debug, Clone, PartialEq)]
pub enum EscalationTrigger {
    /// `cargo test` returned fewer passing tests after this task than
    /// before. Caller computes the diff; this module just packages it.
    NetNegativeTests {
        task_id: String,
        before: u32,
        after: u32,
    },
    /// Plan touches a `dangerous_paths` glob but the goal text doesn't
    /// mention any of the matched components.
    DangerousPathTouched { path: String, goal_mentions: bool },
    /// Diff contains text matching a secrets pattern (high-entropy
    /// string, known API-key prefix, etc.).
    SecretsDetected { kind: String, file: String },
    /// Cumulative spend has crossed the warn (0.8x) threshold.
    CostWarn { spent: f64, cap: f64 },
    /// Cumulative spend has crossed the halt (1.0x) threshold.
    CostHalt { spent: f64, cap: f64 },
    /// Three consecutive failed runs on goals with overlapping keywords.
    /// Caller supplies the run-id chain.
    RepeatedFailures { related_runs: Vec<String> },
    /// A worker tried to modify a license / copyright header.
    LicenseHeaderModified { file: String },
    /// A worker tried to force-push to a branch outside the
    /// `arccode/auto/*` namespace.
    ForcePushOutsideNamespace { branch: String },
    /// R1: an `irreversible` task ran without explicit prompt approval.
    /// Surfaces only when classification + tier disagree.
    IrreversibleTaskUnapproved { task_id: String },
}

impl EscalationTrigger {
    pub fn short_label(&self) -> &'static str {
        match self {
            Self::NetNegativeTests { .. } => "net-negative tests",
            Self::DangerousPathTouched { .. } => "dangerous path touched",
            Self::SecretsDetected { .. } => "secrets detected",
            Self::CostWarn { .. } => "cost warn (>=0.8x)",
            Self::CostHalt { .. } => "cost halt (>=1.0x)",
            Self::RepeatedFailures { .. } => "3 consecutive related failures",
            Self::LicenseHeaderModified { .. } => "license header modified",
            Self::ForcePushOutsideNamespace { .. } => "force-push outside arccode/auto",
            Self::IrreversibleTaskUnapproved { .. } => "irreversible task unapproved",
        }
    }

    /// True when this trigger must block an automatic merge. Every J15
    /// trigger is blocking by definition (they are the non-negotiable
    /// lines) *except* the cost **warning** at 0.8×, which is advisory —
    /// the run continues but the operator is told. `CostHalt` (1.0×) still
    /// blocks.
    pub fn blocks_auto_merge(&self) -> bool {
        !matches!(self, Self::CostWarn { .. })
    }

    /// Plain-text rendering for the R3 escalation packet + the
    /// notification body.
    pub fn render(&self) -> String {
        match self {
            Self::NetNegativeTests {
                task_id,
                before,
                after,
            } => format!(
                "task {task_id} ended with {after} passing tests vs {before} before; \
                 deficit {} test(s)",
                before.saturating_sub(*after)
            ),
            Self::DangerousPathTouched {
                path,
                goal_mentions,
            } => format!(
                "plan touches dangerous path `{path}` but the goal text {}",
                if *goal_mentions {
                    "mentions it (allowed)"
                } else {
                    "does not mention it (escalation required)"
                }
            ),
            Self::SecretsDetected { kind, file } => {
                format!("possible secret of kind `{kind}` in `{file}`")
            }
            Self::CostWarn { spent, cap } => format!(
                "spend ${spent:.2} crossed 80% of cap ${cap:.2}"
            ),
            Self::CostHalt { spent, cap } => format!(
                "spend ${spent:.2} crossed cap ${cap:.2} — halting"
            ),
            Self::RepeatedFailures { related_runs } => format!(
                "three consecutive related runs failed: {}",
                related_runs.join(", ")
            ),
            Self::LicenseHeaderModified { file } => {
                format!("worker tried to modify a license header in `{file}`")
            }
            Self::ForcePushOutsideNamespace { branch } => format!(
                "worker tried to force-push to `{branch}` (outside arccode/auto/*)"
            ),
            Self::IrreversibleTaskUnapproved { task_id } => format!(
                "task {task_id} is classified `irreversible` but ran without explicit prompt approval"
            ),
        }
    }
}

/// Inputs for the runtime check that runs after every task completion.
/// All numeric fields are optional — pass `None` for signals the caller
/// doesn't have access to. Missing signals never trigger; only positive
/// evidence triggers.
pub struct RuntimeSignals<'a> {
    pub state: &'a RunState,
    pub task: Option<&'a Task>,
    pub tests_before: Option<u32>,
    pub tests_after: Option<u32>,
    pub max_usd: f64,
    pub recent_run_outcomes: &'a [(String, bool)], // (run_id, ok)
}

/// Check every J15 trigger that's evaluable from the supplied signals.
/// Returns an empty vec on a clean run.
pub fn check_runtime(signals: &RuntimeSignals<'_>) -> Vec<EscalationTrigger> {
    let mut triggers = Vec::new();

    if let (Some(before), Some(after)) = (signals.tests_before, signals.tests_after) {
        if after < before {
            triggers.push(EscalationTrigger::NetNegativeTests {
                task_id: signals
                    .task
                    .map(|t| t.id.clone())
                    .unwrap_or_else(|| "<unknown>".into()),
                before,
                after,
            });
        }
    }

    if signals.max_usd > 0.0 {
        let spent = signals.state.totals.usd;
        if spent >= signals.max_usd {
            triggers.push(EscalationTrigger::CostHalt {
                spent,
                cap: signals.max_usd,
            });
        } else if spent >= signals.max_usd * 0.8 {
            triggers.push(EscalationTrigger::CostWarn {
                spent,
                cap: signals.max_usd,
            });
        }
    }

    // Repeated failures: tail the most recent up-to-3 outcomes; if all
    // three are failures, trigger.
    if signals.recent_run_outcomes.len() >= 3 {
        let tail = &signals.recent_run_outcomes[signals.recent_run_outcomes.len() - 3..];
        if tail.iter().all(|(_, ok)| !ok) {
            triggers.push(EscalationTrigger::RepeatedFailures {
                related_runs: tail.iter().map(|(id, _)| id.clone()).collect(),
            });
        }
    }

    // R1 — irreversible-task-unapproved check. Surfaced only when we
    // know the task ran (status == InProgress / Review / Done) and its
    // reversibility is irreversible. The approval check is the caller's
    // responsibility; here we just spot the "ran without approval"
    // shape.
    if let Some(t) = signals.task {
        if matches!(t.reversibility, Reversibility::Irreversible)
            && matches!(
                t.status,
                TaskStatus::InProgress | TaskStatus::Review | TaskStatus::Done
            )
        {
            triggers.push(EscalationTrigger::IrreversibleTaskUnapproved {
                task_id: t.id.clone(),
            });
        }
    }

    triggers
}

// ---------------------------------------------------------------------------
// J15 static (plan + diff) triggers
//
// `check_runtime` above covers the numeric/state signals available after a
// task completes (test deltas, spend, failure streaks). The functions below
// cover the signals that live in the *plan* (a dangerous path the goal never
// asked for) or in the *diff* (a leaked secret, a touched license header) or
// in a *git operation* (a force-push outside the pilot's namespace).
// ---------------------------------------------------------------------------

/// License / copyright header markers. A changed diff line containing any
/// of these (case-insensitive) is treated as touching a license header.
const LICENSE_MARKERS: &[&str] = &[
    "copyright",
    "spdx-license-identifier",
    "licensed under",
    "all rights reserved",
    "permission is hereby granted",     // MIT body
    "redistribution and use in source", // BSD body
    "gnu general public license",
    "apache license",
    "mozilla public license",
];

/// True when the goal text plausibly refers to `path` — either the path
/// verbatim or a meaningful component (directory / filename segment) of it.
/// Used to decide whether a `dangerous_paths` hit was *intended* by the user
/// (mentioned in the goal) or sneaked in (escalation required).
pub fn goal_mentions_path(goal: &str, path: &str) -> bool {
    let goal_lc = goal.to_ascii_lowercase();
    let path_lc = path.to_ascii_lowercase();
    if goal_lc.contains(&path_lc) {
        return true;
    }
    // Structural / extension segments carry no signal about *what* is being
    // touched, so a goal that merely says "rust" shouldn't excuse an edit to
    // `crates/auth/src/login.rs`.
    const NOISE: &[&str] = &[
        "src", "crates", "crate", "lib", "mod", "rs", "toml", "md", "txt", "json", "yaml", "yml",
        "tests", "test", "main", "the", "and", "for",
    ];
    for seg in path_lc.split(|c: char| !c.is_ascii_alphanumeric()) {
        if seg.len() < 3 || NOISE.contains(&seg) {
            continue;
        }
        if goal_lc.contains(seg) {
            return true;
        }
    }
    false
}

/// J15 — for each dangerous-path hit (already matched against
/// `[pilot.approval].dangerous_paths` by [`crate::approval::paths_matching`]),
/// raise a [`EscalationTrigger::DangerousPathTouched`] when the goal text
/// doesn't mention it. Hits the goal *does* mention are intentional and don't
/// escalate. Deduplicates by path.
pub fn dangerous_path_triggers(dangerous_hits: &[String], goal: &str) -> Vec<EscalationTrigger> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for path in dangerous_hits {
        if !seen.insert(path.as_str()) {
            continue;
        }
        if !goal_mentions_path(goal, path) {
            out.push(EscalationTrigger::DangerousPathTouched {
                path: path.clone(),
                goal_mentions: false,
            });
        }
    }
    out
}

/// J15 — map the built-in secrets scan ([`crate::security::scan_secrets`])
/// over the added diff lines into [`EscalationTrigger::SecretsDetected`].
/// `added` is `(file, line_text)` for the `+` lines of a diff (sans the `+`).
pub fn secret_triggers(added: &[(String, String)]) -> Vec<EscalationTrigger> {
    crate::security::scan_secrets(added)
        .into_iter()
        .filter(|f| f.kind == "secret")
        .map(|f| EscalationTrigger::SecretsDetected {
            kind: f.message,
            file: f.file.unwrap_or_default(),
        })
        .collect()
}

/// J15 — flag any changed diff line that touches a license / copyright
/// header. `changed` is `(file, line_text)` for every added *or removed* line
/// (a removed header line matters as much as an added one). One trigger per
/// file.
pub fn license_header_triggers(changed: &[(String, String)]) -> Vec<EscalationTrigger> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (file, line) in changed {
        let l = line.to_ascii_lowercase();
        if LICENSE_MARKERS.iter().any(|m| l.contains(m)) && seen.insert(file.clone()) {
            out.push(EscalationTrigger::LicenseHeaderModified { file: file.clone() });
        }
    }
    out
}

/// True when `branch` is inside the pilot's own `arccode/auto/*` namespace,
/// where force-pushes are routine and safe. Tolerates `refs/heads/` and
/// `origin/` prefixes.
pub fn is_pilot_namespace(branch: &str) -> bool {
    let b = branch
        .trim()
        .trim_start_matches("refs/heads/")
        .trim_start_matches("origin/");
    b.starts_with("arccode/auto/")
}

/// J15 — a force-push is allowed only inside the `arccode/auto/*` namespace.
/// Returns a trigger when a force-push targets any other branch. `branch` may
/// be a bare name or a `refs/heads/...` ref.
pub fn force_push_trigger(branch: &str, is_force: bool) -> Option<EscalationTrigger> {
    if is_force && !is_pilot_namespace(branch) {
        Some(EscalationTrigger::ForcePushOutsideNamespace {
            branch: branch.to_string(),
        })
    } else {
        None
    }
}

/// R1 plan-time gate. Given the proposed plan + tier + the
/// classifier's result, returns true when the run MUST request a hard
/// gate regardless of E1's decision. Used by the CLI to override the
/// approval tier when reversibility is high enough.
pub fn r1_requires_hard_gate(plan: &[PlannedTask], tier: PilotTier) -> bool {
    let any_irreversible = plan
        .iter()
        .any(|t| matches!(t.reversibility, Reversibility::Irreversible));
    if any_irreversible {
        return true; // always hard for irreversible tasks
    }
    let any_hard = plan
        .iter()
        .any(|t| matches!(t.reversibility, Reversibility::Hard));
    if any_hard && tier == PilotTier::Copilot {
        return true; // copilot: hard reversibility requires hard gate
    }
    false
}

/// R1 notify-only gate for autopilot. Returns true when the plan
/// should drop to notify-only (even if E1 said auto) because of
/// `hard`-class reversibility on autopilot tier.
pub fn r1_requires_notify_only(plan: &[PlannedTask], tier: PilotTier) -> bool {
    if tier != PilotTier::Autopilot {
        return false;
    }
    plan.iter()
        .any(|t| matches!(t.reversibility, Reversibility::Hard))
}

/// Apply R1 + the E1 classifier together. Returns the *final* approval
/// tier the CLI should honor.
pub fn final_approval_tier(
    plan: &[PlannedTask],
    e1_result: ApprovalTier,
    tier: PilotTier,
) -> ApprovalTier {
    if r1_requires_hard_gate(plan, tier) {
        return ApprovalTier::Hard;
    }
    if r1_requires_notify_only(plan, tier) && e1_result == ApprovalTier::Auto {
        return ApprovalTier::NotifyOnly;
    }
    e1_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Reversibility, Role, RunState, Task, TaskStatus};

    fn empty_state() -> RunState {
        RunState::new("r1", "g", "abc", "arccode/auto/r1")
    }

    fn task_with(id: &str, rev: Reversibility, status: TaskStatus) -> Task {
        let mut t = Task::new(id, Role::Developer, "x");
        t.reversibility = rev;
        t.status = status;
        t
    }

    #[test]
    fn net_negative_tests_triggers() {
        let state = empty_state();
        let task = task_with("t1", Reversibility::Trivial, TaskStatus::Done);
        let signals = RuntimeSignals {
            state: &state,
            task: Some(&task),
            tests_before: Some(120),
            tests_after: Some(115),
            max_usd: 0.0,
            recent_run_outcomes: &[],
        };
        let triggers = check_runtime(&signals);
        assert!(triggers
            .iter()
            .any(|t| matches!(t, EscalationTrigger::NetNegativeTests { .. })));
    }

    #[test]
    fn cost_halt_triggers_at_or_above_cap() {
        let mut state = empty_state();
        state.totals.usd = 10.5;
        let signals = RuntimeSignals {
            state: &state,
            task: None,
            tests_before: None,
            tests_after: None,
            max_usd: 10.0,
            recent_run_outcomes: &[],
        };
        let triggers = check_runtime(&signals);
        assert!(triggers
            .iter()
            .any(|t| matches!(t, EscalationTrigger::CostHalt { .. })));
    }

    #[test]
    fn cost_warn_triggers_at_eighty_percent() {
        let mut state = empty_state();
        state.totals.usd = 8.5;
        let signals = RuntimeSignals {
            state: &state,
            task: None,
            tests_before: None,
            tests_after: None,
            max_usd: 10.0,
            recent_run_outcomes: &[],
        };
        let triggers = check_runtime(&signals);
        assert!(triggers
            .iter()
            .any(|t| matches!(t, EscalationTrigger::CostWarn { .. })));
        // But NOT halt (under cap).
        assert!(!triggers
            .iter()
            .any(|t| matches!(t, EscalationTrigger::CostHalt { .. })));
    }

    #[test]
    fn repeated_failures_trigger_at_three_in_a_row() {
        let state = empty_state();
        let recent = vec![
            ("r-1".into(), false),
            ("r-2".into(), false),
            ("r-3".into(), false),
        ];
        let signals = RuntimeSignals {
            state: &state,
            task: None,
            tests_before: None,
            tests_after: None,
            max_usd: 0.0,
            recent_run_outcomes: &recent,
        };
        let triggers = check_runtime(&signals);
        assert!(triggers
            .iter()
            .any(|t| matches!(t, EscalationTrigger::RepeatedFailures { .. })));
    }

    #[test]
    fn repeated_failures_dont_trigger_when_last_run_succeeded() {
        let state = empty_state();
        let recent = vec![
            ("r-1".into(), false),
            ("r-2".into(), false),
            ("r-3".into(), true),
        ];
        let signals = RuntimeSignals {
            state: &state,
            task: None,
            tests_before: None,
            tests_after: None,
            max_usd: 0.0,
            recent_run_outcomes: &recent,
        };
        let triggers = check_runtime(&signals);
        assert!(!triggers
            .iter()
            .any(|t| matches!(t, EscalationTrigger::RepeatedFailures { .. })));
    }

    #[test]
    fn irreversible_task_running_surfaces() {
        let state = empty_state();
        let task = task_with("t1", Reversibility::Irreversible, TaskStatus::InProgress);
        let signals = RuntimeSignals {
            state: &state,
            task: Some(&task),
            tests_before: None,
            tests_after: None,
            max_usd: 0.0,
            recent_run_outcomes: &[],
        };
        let triggers = check_runtime(&signals);
        assert!(triggers
            .iter()
            .any(|t| matches!(t, EscalationTrigger::IrreversibleTaskUnapproved { .. })));
    }

    #[test]
    fn r1_irreversible_always_forces_hard() {
        use crate::planner::PlannedTask;
        let plan = vec![PlannedTask {
            id: "t1".into(),
            role: Role::Developer,
            title: "drop column".into(),
            goal: "".into(),
            deps: vec![],
            writes: vec![],
            acceptance: vec![],
            reversibility: Reversibility::Irreversible,
            reversibility_reason: Some("DROP TABLE users".into()),
        }];
        for tier in [PilotTier::Assist, PilotTier::Copilot, PilotTier::Autopilot] {
            assert!(r1_requires_hard_gate(&plan, tier), "tier={tier:?}");
        }
    }

    #[test]
    fn r1_hard_only_blocks_copilot_strictly() {
        use crate::planner::PlannedTask;
        let plan = vec![PlannedTask {
            id: "t1".into(),
            role: Role::Developer,
            title: "cargo update".into(),
            goal: "".into(),
            deps: vec![],
            writes: vec![],
            acceptance: vec![],
            reversibility: Reversibility::Hard,
            reversibility_reason: None,
        }];
        assert!(r1_requires_hard_gate(&plan, PilotTier::Copilot));
        assert!(!r1_requires_hard_gate(&plan, PilotTier::Autopilot));
        // Autopilot drops to notify-only, not hard.
        assert!(r1_requires_notify_only(&plan, PilotTier::Autopilot));
    }

    #[test]
    fn final_approval_tier_layers_r1_over_e1() {
        use crate::planner::PlannedTask;
        let plan = vec![PlannedTask {
            id: "t1".into(),
            role: Role::Developer,
            title: "drop".into(),
            goal: "".into(),
            deps: vec![],
            writes: vec![],
            acceptance: vec![],
            reversibility: Reversibility::Irreversible,
            reversibility_reason: None,
        }];
        assert_eq!(
            final_approval_tier(&plan, ApprovalTier::Auto, PilotTier::Autopilot),
            ApprovalTier::Hard,
            "irreversible promotes Auto → Hard on every tier"
        );
    }

    #[test]
    fn final_approval_tier_passes_through_when_no_r1_concern() {
        use crate::planner::PlannedTask;
        let plan = vec![PlannedTask {
            id: "t1".into(),
            role: Role::Developer,
            title: "tweak docs".into(),
            goal: "".into(),
            deps: vec![],
            writes: vec![],
            acceptance: vec![],
            reversibility: Reversibility::Trivial,
            reversibility_reason: None,
        }];
        assert_eq!(
            final_approval_tier(&plan, ApprovalTier::Auto, PilotTier::Autopilot),
            ApprovalTier::Auto,
        );
    }

    #[test]
    fn trigger_renders_human_readable() {
        let t = EscalationTrigger::CostHalt {
            spent: 12.0,
            cap: 10.0,
        };
        let s = t.render();
        assert!(s.contains("$12.00"));
        assert!(s.contains("$10.00"));
    }

    // --- J15 static triggers --------------------------------------------

    #[test]
    fn goal_mention_matches_meaningful_segment() {
        assert!(goal_mentions_path(
            "harden the auth login flow",
            "crates/auth/src/login.rs"
        ));
        assert!(goal_mentions_path(
            "tweak the github actions workflow",
            ".github/workflows/ci.yml"
        ));
        // Verbatim path mention.
        assert!(goal_mentions_path("edit Cargo.lock by hand", "Cargo.lock"));
    }

    #[test]
    fn goal_mention_ignores_structural_noise() {
        // "rust" / "src" don't excuse touching an auth file.
        assert!(!goal_mentions_path(
            "do some rust cleanup in src",
            "crates/auth/src/login.rs"
        ));
        assert!(!goal_mentions_path("update the readme", "Cargo.lock"));
    }

    #[test]
    fn dangerous_path_triggers_only_when_goal_silent() {
        let hits = vec![
            "crates/auth/src/login.rs".to_string(),
            "Cargo.lock".to_string(),
        ];
        // Goal mentions auth → only Cargo.lock escalates.
        let trips = dangerous_path_triggers(&hits, "refactor the auth module");
        assert_eq!(trips.len(), 1);
        assert!(matches!(
            &trips[0],
            EscalationTrigger::DangerousPathTouched { path, goal_mentions: false } if path == "Cargo.lock"
        ));
    }

    #[test]
    fn dangerous_path_triggers_dedupe() {
        let hits = vec![".github/ci.yml".to_string(), ".github/ci.yml".to_string()];
        let trips = dangerous_path_triggers(&hits, "unrelated goal");
        assert_eq!(trips.len(), 1);
    }

    #[test]
    fn secret_triggers_from_added_lines() {
        let added = vec![(
            "config.rs".to_string(),
            r#"let k = "AKIAIOSFODNN7EXAMPLE";"#.to_string(),
        )];
        let trips = secret_triggers(&added);
        assert!(trips.iter().any(
            |t| matches!(t, EscalationTrigger::SecretsDetected { file, .. } if file == "config.rs")
        ));
    }

    #[test]
    fn secret_triggers_clean_on_prose() {
        let added = vec![("README.md".to_string(), "An ordinary sentence.".to_string())];
        assert!(secret_triggers(&added).is_empty());
    }

    #[test]
    fn license_header_triggers_flag_per_file() {
        let changed = vec![
            (
                "src/lib.rs".to_string(),
                "// SPDX-License-Identifier: MIT".to_string(),
            ),
            (
                "src/lib.rs".to_string(),
                "// Copyright 2026 Someone".to_string(),
            ),
            ("src/main.rs".to_string(), "let x = 1;".to_string()),
        ];
        let trips = license_header_triggers(&changed);
        // Two markers, same file → one trigger.
        assert_eq!(trips.len(), 1);
        assert!(matches!(
            &trips[0],
            EscalationTrigger::LicenseHeaderModified { file } if file == "src/lib.rs"
        ));
    }

    #[test]
    fn force_push_allowed_inside_pilot_namespace() {
        assert!(force_push_trigger("arccode/auto/2026-06-02-x", true).is_none());
        assert!(force_push_trigger("refs/heads/arccode/auto/r1", true).is_none());
        // Non-force push to any branch is fine.
        assert!(force_push_trigger("main", false).is_none());
    }

    #[test]
    fn force_push_outside_namespace_triggers() {
        let t = force_push_trigger("main", true);
        assert!(matches!(
            t,
            Some(EscalationTrigger::ForcePushOutsideNamespace { ref branch }) if branch == "main"
        ));
    }

    #[test]
    fn cost_warn_is_advisory_others_block() {
        assert!(!EscalationTrigger::CostWarn {
            spent: 8.0,
            cap: 10.0
        }
        .blocks_auto_merge());
        assert!(EscalationTrigger::CostHalt {
            spent: 11.0,
            cap: 10.0
        }
        .blocks_auto_merge());
        assert!(EscalationTrigger::SecretsDetected {
            kind: "x".into(),
            file: "y".into()
        }
        .blocks_auto_merge());
        assert!(EscalationTrigger::LicenseHeaderModified { file: "z".into() }.blocks_auto_merge());
    }
}
