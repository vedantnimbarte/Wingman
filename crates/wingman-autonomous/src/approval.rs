//! E1 — trust-tiered plan approval.
//!
//! Classifies a proposed plan into one of three tiers (auto / notify-only /
//! hard gate) based on cost estimate, task count, write-set globs, and
//! dangerous-paths matches. Replaces the unconditional y/e/n prompt
//! that ships in M1 for copilot+ tiers.
//!
//! Pure-function design: callers build a [`ClassifyInputs`], get back an
//! [`ApprovalTier`] + a [`ClassificationReport`], then the CLI decides
//! whether to prompt, notify, or proceed silently.

use std::fmt;

use globset::{Glob, GlobSet, GlobSetBuilder};
use wingman_config::{PilotApprovalConfig, PilotTier};

use crate::planner::PlannedTask;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalTier {
    /// Plan is low-risk by every measure. Proceed silently.
    Auto,
    /// Plan is medium-risk. Print a notification with the plan summary
    /// and proceed unless the user vetoes within
    /// `notify_only_window_secs`.
    NotifyOnly,
    /// Plan is high-risk or the user forced `--review`. Fall back to
    /// the y/e/n prompt.
    Hard,
}

impl fmt::Display for ApprovalTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::NotifyOnly => "notify-only",
            Self::Hard => "hard-gate",
        })
    }
}

/// All the inputs the classifier needs. Built by the CLI from the
/// resolved plan + config; kept separate from a giant function signature
/// so tests don't have to construct a 7-arg call.
pub struct ClassifyInputs<'a> {
    pub plan: &'a [PlannedTask],
    pub config: &'a PilotApprovalConfig,
    pub tier: PilotTier,
    /// True when the user passed `--yes`.
    pub force_auto: bool,
    /// True when the user passed `--review`.
    pub force_hard: bool,
    /// J9 cost/risk band for this plan. When present, auto-approval gates on
    /// its **upper bound** (worst-case) — the only honest basis for firing
    /// without asking — and the reported cost is the band's point estimate.
    /// `None` falls back to the static per-role point estimate
    /// ([`estimate_plan_cost_usd`]).
    pub estimate: Option<&'a crate::estimate::Estimate>,
}

/// Human-readable justification of why the classifier landed where it
/// did. Surfaced in the notify-only message and the audit log.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassificationReport {
    pub tier: ApprovalTier,
    /// One-line summary of the deciding factor(s).
    pub reason: String,
    /// Estimated cost in USD (rough heuristic — see [`estimate_plan_cost_usd`]).
    pub estimated_usd: f64,
    /// Paths in the plan's writes set that match
    /// `[pilot.approval].dangerous_paths`. Empty when the plan is safe.
    pub dangerous_hits: Vec<String>,
    /// Paths that don't match any entry in `auto_approve_globs`.
    /// Non-empty means at least one write is outside the auto-approve
    /// allowlist.
    pub out_of_allowlist: Vec<String>,
}

/// Classify the plan. Pure function; performs no I/O. The caller is
/// responsible for surfacing the result to the user (notification or
/// prompt).
pub fn classify(inputs: ClassifyInputs<'_>) -> ClassificationReport {
    // Reported cost: the J9 band's point estimate when we have one, else the
    // static per-role placeholder. Used for display/audit on every path.
    let reported_usd = inputs
        .estimate
        .map(|e| e.usd_point)
        .unwrap_or_else(|| estimate_plan_cost_usd(inputs.plan));

    // Override precedence: --review wins over --yes wins over tier
    // defaults. assist always lands in Hard regardless of risk.
    if inputs.force_hard {
        return ClassificationReport {
            tier: ApprovalTier::Hard,
            reason: "--review forced hard gate".into(),
            estimated_usd: reported_usd,
            dangerous_hits: matches_globs(inputs.plan, &inputs.config.dangerous_paths),
            out_of_allowlist: writes_outside_allowlist(
                inputs.plan,
                &inputs.config.auto_approve_globs,
            ),
        };
    }
    if inputs.tier == PilotTier::Assist {
        return ClassificationReport {
            tier: ApprovalTier::Hard,
            reason: "assist tier always uses hard gate".into(),
            estimated_usd: reported_usd,
            dangerous_hits: matches_globs(inputs.plan, &inputs.config.dangerous_paths),
            out_of_allowlist: writes_outside_allowlist(
                inputs.plan,
                &inputs.config.auto_approve_globs,
            ),
        };
    }
    if inputs.force_auto {
        // --yes is the user explicitly skipping the gate. We still
        // populate the report so the audit log records why.
        return ClassificationReport {
            tier: ApprovalTier::Auto,
            reason: "--yes forced auto-approve".into(),
            estimated_usd: reported_usd,
            dangerous_hits: matches_globs(inputs.plan, &inputs.config.dangerous_paths),
            out_of_allowlist: writes_outside_allowlist(
                inputs.plan,
                &inputs.config.auto_approve_globs,
            ),
        };
    }

    // Compute the signals.
    let estimated_usd = reported_usd;
    let dangerous_hits = matches_globs(inputs.plan, &inputs.config.dangerous_paths);
    let out_of_allowlist = writes_outside_allowlist(inputs.plan, &inputs.config.auto_approve_globs);
    let task_count = inputs.plan.len() as u32;

    // Decision tree.
    if !dangerous_hits.is_empty() {
        return ClassificationReport {
            tier: ApprovalTier::Hard,
            reason: format!(
                "plan touches dangerous_paths ({} hit{}); requires hard gate",
                dangerous_hits.len(),
                if dangerous_hits.len() == 1 { "" } else { "s" }
            ),
            estimated_usd,
            dangerous_hits,
            out_of_allowlist,
        };
    }

    // Auto-approval gates on the worst-case cost when we have a band (J9
    // §"confidence bands matter more"): auto-fire only when even the upper
    // bound clears the cap. With no band, fall back to the point estimate.
    let cap = inputs.config.auto_approve_usd;
    let cheap_enough = match inputs.estimate {
        Some(e) => e.upper_bound_under(cap),
        None => estimated_usd < cap,
    };
    let small_enough = task_count <= inputs.config.auto_approve_max_tasks;
    let allowlisted = out_of_allowlist.is_empty();

    if cheap_enough && small_enough && allowlisted {
        return ClassificationReport {
            tier: ApprovalTier::Auto,
            reason: format!(
                "plan is low-risk: {task_count} tasks, est. ${estimated_usd:.2} ≤ ${cap:.2}, every write matches auto_approve_globs",
                cap = inputs.config.auto_approve_usd
            ),
            estimated_usd,
            dangerous_hits,
            out_of_allowlist,
        };
    }

    // Anything else lands in notify-only on copilot/autopilot tiers.
    // The notify window gives the user the chance to veto for medium
    // risk; the alternative is a full hard prompt for every plan,
    // which is the M1 behaviour.
    let mut reasons = Vec::new();
    if !cheap_enough {
        // Report the figure the gate actually tripped on: the band's upper
        // bound when we have one, else the point estimate.
        let tripped = inputs.estimate.map(|e| e.usd_high).unwrap_or(estimated_usd);
        let basis = if inputs.estimate.is_some() {
            "worst-case cost"
        } else {
            "estimated cost"
        };
        reasons.push(format!("{basis} ${tripped:.2} > ${cap:.2}"));
    }
    if !small_enough {
        reasons.push(format!(
            "{task_count} tasks > {cap}",
            cap = inputs.config.auto_approve_max_tasks
        ));
    }
    if !allowlisted {
        reasons.push(format!(
            "{} write(s) outside auto_approve_globs",
            out_of_allowlist.len()
        ));
    }
    ClassificationReport {
        tier: ApprovalTier::NotifyOnly,
        reason: format!("plan is medium-risk: {}", reasons.join("; ")),
        estimated_usd,
        dangerous_hits,
        out_of_allowlist,
    }
}

/// Rough cost heuristic: $0.05 per developer/refactorer task, $0.03 per
/// designer/tester/reviewer, $0.02 per merge-fixer/custom. Numbers
/// picked to roughly mirror typical Haiku/Sonnet usage on small tasks.
/// Real cost-estimation (J9) reads per-role stats from
/// `~/.wingman/stats.jsonl`; this is the placeholder until E6 lands.
pub fn estimate_plan_cost_usd(plan: &[PlannedTask]) -> f64 {
    use crate::model::Role;
    plan.iter()
        .map(|t| match t.role {
            Role::Developer | Role::Refactorer => 0.05,
            Role::Designer | Role::Tester | Role::Reviewer => 0.03,
            Role::MergeFixer | Role::Custom(_) => 0.02,
        })
        .sum()
}

/// Return every write-path in the plan that matches any of the named
/// globs. Patterns that fail to compile are skipped with a warning.
pub fn matches_globs(plan: &[PlannedTask], patterns: &[String]) -> Vec<String> {
    let paths: Vec<String> = plan.iter().flat_map(|t| t.writes.iter().cloned()).collect();
    paths_matching(&paths, patterns)
}

/// Return every entry in `paths` that matches any of the named globs,
/// sorted + deduped. Shared by [`matches_globs`] and the J15 dangerous-path
/// escalation check (which works over a run's recorded writes, not a
/// [`PlannedTask`] list). Patterns that fail to compile are skipped.
pub fn paths_matching(paths: &[String], patterns: &[String]) -> Vec<String> {
    let Some(set) = build_globset(patterns) else {
        return Vec::new();
    };
    let mut hits: Vec<String> = paths.iter().filter(|p| set.is_match(p)).cloned().collect();
    hits.sort();
    hits.dedup();
    hits
}

/// Return every write-path that does NOT match the allowlist globs.
/// Empty allowlist means everything is out-of-allowlist (conservative).
pub fn writes_outside_allowlist(plan: &[PlannedTask], allowlist: &[String]) -> Vec<String> {
    let Some(set) = build_globset(allowlist) else {
        // No allowlist or unparseable: treat everything as outside.
        return plan
            .iter()
            .flat_map(|t| t.writes.iter().cloned())
            .collect::<Vec<_>>();
    };
    let mut out = Vec::new();
    for t in plan {
        for w in &t.writes {
            if !set.is_match(w) {
                out.push(w.clone());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn build_globset(patterns: &[String]) -> Option<GlobSet> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = GlobSetBuilder::new();
    let mut added = 0;
    for p in patterns {
        match Glob::new(p) {
            Ok(g) => {
                builder.add(g);
                added += 1;
            }
            Err(e) => {
                tracing::warn!(target: "pilot::approval", pattern = %p, error = %e, "skipping invalid glob");
            }
        }
    }
    if added == 0 {
        return None;
    }
    builder.build().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Reversibility, Role};
    use wingman_config::PilotApprovalConfig;

    fn task(id: &str, role: Role, writes: Vec<&str>) -> PlannedTask {
        PlannedTask {
            id: id.into(),
            role,
            title: format!("task {id}"),
            goal: String::new(),
            deps: vec![],
            writes: writes.into_iter().map(String::from).collect(),
            acceptance: vec![],
            reversibility: Reversibility::default(),
            reversibility_reason: None,
        }
    }

    fn default_config() -> PilotApprovalConfig {
        PilotApprovalConfig::default()
    }

    #[test]
    fn estimate_uses_role_specific_rates() {
        let plan = vec![
            task("t1", Role::Developer, vec![]),
            task("t2", Role::Tester, vec![]),
            task("t3", Role::Reviewer, vec![]),
            task("t4", Role::MergeFixer, vec![]),
        ];
        let est = estimate_plan_cost_usd(&plan);
        // 0.05 + 0.03 + 0.03 + 0.02 = 0.13
        assert!((est - 0.13).abs() < 1e-9, "got {est}");
    }

    #[test]
    fn matches_globs_finds_dangerous_paths() {
        let plan = vec![
            task("t1", Role::Developer, vec!["crates/auth/src/login.rs"]),
            task("t2", Role::Developer, vec!["docs/setup.md"]),
        ];
        let patterns = vec!["**/auth/**".into()];
        let hits = matches_globs(&plan, &patterns);
        assert_eq!(hits, vec!["crates/auth/src/login.rs"]);
    }

    #[test]
    fn writes_outside_allowlist_finds_strays() {
        let plan = vec![
            task(
                "t1",
                Role::Developer,
                vec!["crates/wingman-cli/src/main.rs"],
            ),
            task("t2", Role::Developer, vec!["/etc/passwd"]),
            task("t3", Role::Developer, vec!["docs/README.md"]),
        ];
        let allow = vec!["crates/**/*.rs".into(), "docs/**".into()];
        let outside = writes_outside_allowlist(&plan, &allow);
        assert_eq!(outside, vec!["/etc/passwd"]);
    }

    #[test]
    fn dangerous_path_forces_hard_gate_on_copilot_tier() {
        let plan = vec![task(
            "t1",
            Role::Developer,
            vec!["crates/auth/src/login.rs"],
        )];
        let cfg = default_config();
        let report = classify(ClassifyInputs {
            plan: &plan,
            config: &cfg,
            tier: PilotTier::Copilot,
            force_auto: false,
            force_hard: false,
            estimate: None,
        });
        assert_eq!(report.tier, ApprovalTier::Hard);
        assert!(report.reason.contains("dangerous_paths"));
    }

    #[test]
    fn assist_tier_always_hard_gate() {
        let plan = vec![task("t1", Role::Developer, vec!["docs/x.md"])];
        let report = classify(ClassifyInputs {
            plan: &plan,
            config: &default_config(),
            tier: PilotTier::Assist,
            force_auto: false,
            force_hard: false,
            estimate: None,
        });
        assert_eq!(report.tier, ApprovalTier::Hard);
        assert!(report.reason.contains("assist tier"));
    }

    #[test]
    fn yes_flag_forces_auto_even_at_assist_tier() {
        // --review wins over --yes, but on its own --yes should bypass
        // even the assist tier hard gate. (The CLI separates these
        // concerns; we just verify the classifier's precedence.)
        // Actually re-reading plan.md: assist tier sets `Hard plan
        // approval (Phase 2)` ON by default. --yes is a per-run
        // override. The classifier's job is to surface the tier; if
        // assist forces hard, --yes shouldn't bypass.
        // Resolved: --review > assist-tier > --yes > tier defaults.
        let plan = vec![task("t1", Role::Developer, vec!["docs/x.md"])];
        let report = classify(ClassifyInputs {
            plan: &plan,
            config: &default_config(),
            tier: PilotTier::Assist,
            force_auto: true,
            force_hard: false,
            estimate: None,
        });
        // Plan.md is ambiguous; we pick "assist tier wins" (safer).
        assert_eq!(report.tier, ApprovalTier::Hard);
    }

    #[test]
    fn review_flag_always_forces_hard() {
        let plan = vec![task("t1", Role::Developer, vec!["docs/x.md"])];
        let report = classify(ClassifyInputs {
            plan: &plan,
            config: &default_config(),
            tier: PilotTier::Autopilot,
            force_auto: true,
            force_hard: true,
            estimate: None,
        });
        assert_eq!(report.tier, ApprovalTier::Hard);
        assert!(report.reason.contains("--review"));
    }

    #[test]
    fn small_cheap_allowlisted_plan_is_auto() {
        let plan = vec![
            task(
                "t1",
                Role::Developer,
                vec!["crates/wingman-cli/src/args.rs"],
            ),
            task("t2", Role::Tester, vec!["crates/wingman-cli/tests/x.rs"]),
        ];
        let report = classify(ClassifyInputs {
            plan: &plan,
            config: &default_config(),
            tier: PilotTier::Copilot,
            force_auto: false,
            force_hard: false,
            estimate: None,
        });
        assert_eq!(report.tier, ApprovalTier::Auto);
        assert!(report.dangerous_hits.is_empty());
        assert!(report.out_of_allowlist.is_empty());
    }

    #[test]
    fn over_budget_plan_drops_to_notify_only() {
        // 30 developer tasks at $0.05 = $1.50 > $1.00 default cap;
        // also > 5 task cap. Should land in NotifyOnly (not Hard,
        // because no dangerous paths).
        let plan: Vec<PlannedTask> = (1..=30)
            .map(|i| {
                task(
                    &format!("t{i}"),
                    Role::Developer,
                    vec!["crates/wingman-cli/src/main.rs"],
                )
            })
            .collect();
        let report = classify(ClassifyInputs {
            plan: &plan,
            config: &default_config(),
            tier: PilotTier::Copilot,
            force_auto: false,
            force_hard: false,
            estimate: None,
        });
        assert_eq!(report.tier, ApprovalTier::NotifyOnly);
        assert!(report.reason.contains("estimated cost") || report.reason.contains("tasks >"));
    }

    #[test]
    fn out_of_allowlist_writes_drop_to_notify_only() {
        let plan = vec![task("t1", Role::Developer, vec!["/etc/hostname"])];
        let report = classify(ClassifyInputs {
            plan: &plan,
            config: &default_config(),
            tier: PilotTier::Copilot,
            force_auto: false,
            force_hard: false,
            estimate: None,
        });
        assert_eq!(report.tier, ApprovalTier::NotifyOnly);
        assert!(report.reason.contains("auto_approve_globs"));
    }

    fn band(point: f64, high: f64) -> crate::estimate::Estimate {
        crate::estimate::Estimate {
            task_count: 1,
            usd_low: 0.0,
            usd_point: point,
            usd_high: high,
            wall_min_low: 1.0,
            wall_min_high: 2.0,
            risk: crate::estimate::RiskLevel::Low,
            confidence: crate::estimate::Confidence::Low,
            sample_count: 0,
        }
    }

    #[test]
    fn band_upper_bound_gates_auto_approval() {
        // Point estimate is well under the $1 cap, but the worst-case upper
        // bound exceeds it — auto-approval must NOT fire (J9 §upper bound).
        let plan = vec![task("t1", Role::Developer, vec!["crates/x/src/a.rs"])];
        let cfg = PilotApprovalConfig {
            auto_approve_globs: vec!["crates/**/*.rs".into()],
            ..default_config()
        };
        let est = band(0.30, 1.20);
        let report = classify(ClassifyInputs {
            plan: &plan,
            config: &cfg,
            tier: PilotTier::Copilot,
            force_auto: false,
            force_hard: false,
            estimate: Some(&est),
        });
        assert_eq!(report.tier, ApprovalTier::NotifyOnly);
        assert!(report.reason.contains("worst-case cost"));
        // Reported cost is the point estimate, not the placeholder.
        assert!((report.estimated_usd - 0.30).abs() < 1e-9);

        // Same plan, band fully under the cap → auto-approves.
        let est = band(0.30, 0.90);
        let report = classify(ClassifyInputs {
            plan: &plan,
            config: &cfg,
            tier: PilotTier::Copilot,
            force_auto: false,
            force_hard: false,
            estimate: Some(&est),
        });
        assert_eq!(report.tier, ApprovalTier::Auto);
    }
}
