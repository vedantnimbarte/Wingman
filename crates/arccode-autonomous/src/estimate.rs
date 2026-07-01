//! J9 — upfront cost / time / risk estimation with confidence.
//!
//! The M2 placeholder ([`crate::approval::estimate_plan_cost_usd`]) returns
//! a single point estimate from a static per-role rate table. That's fine
//! for a rough gate but useless for honest auto-approval: E1 should only
//! auto-fire when the *upper bound* of the cost range is under the cap, and
//! that requires bands, not a point.
//!
//! This module produces `low / point / high` USD bands, a wall-clock
//! range, a coarse risk level, and a confidence rating — derived from
//! historical per-role cost samples ([`CostSamples`], fed from past runs'
//! `agent.usd` events) with a graceful fallback to the static rates when
//! there's no history. Confidence is the headline: a tight band with 12
//! similar past runs means something; a wide band from priors does not.

use std::collections::BTreeMap;

use crate::model::{Role, RunState};
use crate::planner::PlannedTask;

/// Historical per-role USD cost samples, gathered from past runs.
#[derive(Debug, Clone, Default)]
pub struct CostSamples {
    /// `Role::as_str()` → observed per-task USD costs.
    pub per_role: BTreeMap<String, Vec<f64>>,
}

impl CostSamples {
    pub fn add(&mut self, role: &str, usd: f64) {
        self.per_role.entry(role.to_string()).or_default().push(usd);
    }

    fn samples(&self, role: &str) -> Option<&[f64]> {
        self.per_role
            .get(role)
            .map(|v| v.as_slice())
            .filter(|v| !v.is_empty())
    }

    /// Total number of samples across all roles.
    pub fn total(&self) -> usize {
        self.per_role.values().map(Vec::len).sum()
    }
}

/// J9 — build per-role cost samples from past run snapshots, so the
/// estimator can produce tight, high-confidence bands once a project has
/// history (instead of falling back to the static priors).
///
/// Each task that recorded a positive USD spend contributes one sample to
/// its role's bucket. A task's `usd` field is itself the replayed sum of the
/// `agent.usd` events attributed to it (see [`crate::model::apply`]), so
/// this is exactly "samples fed from past runs' `agent.usd` events" — just
/// pre-aggregated per task at the granularity the estimator reasons about
/// (one sample == one task's all-in cost).
pub fn cost_samples_from_runs<'a>(runs: impl IntoIterator<Item = &'a RunState>) -> CostSamples {
    let mut samples = CostSamples::default();
    for run in runs {
        for task in &run.tasks {
            if task.usd > 0.0 {
                samples.add(task.role.as_str(), task.usd);
            }
        }
    }
    samples
}

/// Static per-role fallback rate (mirrors the M2 placeholder).
fn role_rate(role: &Role) -> f64 {
    match role {
        Role::Developer | Role::Refactorer => 0.05,
        Role::Designer | Role::Tester | Role::Reviewer => 0.03,
        Role::MergeFixer | Role::Custom(_) => 0.02,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// A full plan estimate.
#[derive(Debug, Clone, PartialEq)]
pub struct Estimate {
    pub task_count: u32,
    pub usd_low: f64,
    pub usd_point: f64,
    pub usd_high: f64,
    pub wall_min_low: f64,
    pub wall_min_high: f64,
    pub risk: RiskLevel,
    pub confidence: Confidence,
    /// How many historical samples backed the estimate (sum across roles).
    pub sample_count: usize,
}

impl Estimate {
    /// True when the worst-case cost is under `cap` — the only safe basis
    /// for E1 auto-approval (J9 §"confidence bands matter more").
    pub fn upper_bound_under(&self, cap: f64) -> bool {
        self.usd_high < cap
    }

    /// Render the plan.md-style banner.
    pub fn render(&self) -> String {
        format!(
            "Estimated: {} task(s) · {:.0}–{:.0} min wall · ${:.2}–${:.2} (≈${:.2}) · risk: {}\nConfidence: {} ({} similar past sample(s))",
            self.task_count,
            self.wall_min_low,
            self.wall_min_high,
            self.usd_low,
            self.usd_high,
            self.usd_point,
            self.risk.as_str(),
            self.confidence.as_str(),
            self.sample_count,
        )
    }
}

/// Linear-interpolated percentile of a sorted slice. `q` in `[0,1]`.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = q * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    let frac = rank - lo as f64;
    sorted[lo] + (sorted[hi] - sorted[lo]) * frac
}

/// Estimate one task's `(low, point, high)` USD cost.
fn estimate_task(role: &Role, samples: &CostSamples) -> (f64, f64, f64) {
    match samples.samples(role.as_str()) {
        Some(s) if s.len() >= 3 => {
            let mut sorted = s.to_vec();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            (
                percentile(&sorted, 0.20),
                percentile(&sorted, 0.50),
                percentile(&sorted, 0.80),
            )
        }
        // 1–2 samples: use the mean with a wide ±60% band.
        Some(s) => {
            let mean = s.iter().sum::<f64>() / s.len() as f64;
            (mean * 0.4, mean, mean * 1.6)
        }
        // No history: static rate, ±50% band.
        None => {
            let rate = role_rate(role);
            (rate * 0.5, rate, rate * 1.5)
        }
    }
}

/// Coarse risk from the plan's reversibility classification (R1).
pub fn risk_level(plan: &[PlannedTask]) -> RiskLevel {
    use crate::model::Reversibility::*;
    if plan.iter().any(|t| matches!(t.reversibility, Irreversible)) {
        RiskLevel::High
    } else if plan.iter().any(|t| matches!(t.reversibility, Hard)) {
        RiskLevel::Medium
    } else {
        RiskLevel::Low
    }
}

/// Confidence from per-role sample coverage of the plan.
fn confidence(plan: &[PlannedTask], samples: &CostSamples) -> Confidence {
    if plan.is_empty() {
        return Confidence::Low;
    }
    let mut min_samples = usize::MAX;
    for t in plan {
        let n = samples
            .samples(t.role.as_str())
            .map(|s| s.len())
            .unwrap_or(0);
        min_samples = min_samples.min(n);
    }
    match min_samples {
        n if n >= 8 => Confidence::High,
        n if n >= 3 => Confidence::Medium,
        _ => Confidence::Low,
    }
}

/// Full plan estimate. `concurrency` is the effective worker cap; wall-time
/// assumes tasks pack into `ceil(tasks / concurrency)` sequential waves at
/// 2–8 minutes per wave.
pub fn estimate_plan(plan: &[PlannedTask], samples: &CostSamples, concurrency: u32) -> Estimate {
    let mut usd_low = 0.0;
    let mut usd_point = 0.0;
    let mut usd_high = 0.0;
    let mut sample_count = 0;
    for t in plan {
        let (lo, pt, hi) = estimate_task(&t.role, samples);
        usd_low += lo;
        usd_point += pt;
        usd_high += hi;
        sample_count += samples
            .samples(t.role.as_str())
            .map(|s| s.len())
            .unwrap_or(0);
    }

    let conc = concurrency.max(1);
    let waves = plan.len().div_ceil(conc as usize) as f64;
    let wall_min_low = waves * 2.0;
    let wall_min_high = waves * 8.0;

    Estimate {
        task_count: plan.len() as u32,
        usd_low,
        usd_point,
        usd_high,
        wall_min_low,
        wall_min_high,
        risk: risk_level(plan),
        confidence: confidence(plan, samples),
        sample_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Reversibility, Role};

    fn task(role: Role, rev: Reversibility) -> PlannedTask {
        PlannedTask {
            id: "t".into(),
            role,
            title: "t".into(),
            goal: String::new(),
            deps: vec![],
            writes: vec![],
            acceptance: vec![],
            reversibility: rev,
            reversibility_reason: None,
        }
    }

    #[test]
    fn percentile_interpolates() {
        let s = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&s, 0.0), 1.0);
        assert_eq!(percentile(&s, 1.0), 5.0);
        assert_eq!(percentile(&s, 0.5), 3.0);
    }

    #[test]
    fn no_history_uses_static_rate_low_confidence() {
        let plan = vec![task(Role::Developer, Reversibility::Trivial)];
        let est = estimate_plan(&plan, &CostSamples::default(), 4);
        assert_eq!(est.confidence, Confidence::Low);
        assert_eq!(est.task_count, 1);
        // band straddles the static 0.05 rate.
        assert!(est.usd_low < 0.05 && est.usd_high > 0.05);
        assert_eq!(est.sample_count, 0);
    }

    #[test]
    fn rich_history_tightens_band_and_raises_confidence() {
        let mut samples = CostSamples::default();
        for _ in 0..10 {
            samples.add("developer", 0.10);
        }
        let plan = vec![task(Role::Developer, Reversibility::Trivial)];
        let est = estimate_plan(&plan, &samples, 4);
        assert_eq!(est.confidence, Confidence::High);
        // All samples identical → tight band around 0.10.
        assert!((est.usd_point - 0.10).abs() < 1e-9);
        assert!((est.usd_high - 0.10).abs() < 1e-9);
        assert_eq!(est.sample_count, 10);
    }

    #[test]
    fn medium_confidence_at_three_samples() {
        let mut samples = CostSamples::default();
        for c in [0.08, 0.10, 0.12] {
            samples.add("developer", c);
        }
        let plan = vec![task(Role::Developer, Reversibility::Trivial)];
        let est = estimate_plan(&plan, &samples, 4);
        assert_eq!(est.confidence, Confidence::Medium);
    }

    #[test]
    fn confidence_is_min_across_roles() {
        let mut samples = CostSamples::default();
        for _ in 0..10 {
            samples.add("developer", 0.10);
        }
        // tester has no samples → overall confidence stays Low.
        let plan = vec![
            task(Role::Developer, Reversibility::Trivial),
            task(Role::Tester, Reversibility::Trivial),
        ];
        let est = estimate_plan(&plan, &samples, 4);
        assert_eq!(est.confidence, Confidence::Low);
    }

    #[test]
    fn risk_tracks_reversibility() {
        assert_eq!(
            risk_level(&[task(Role::Developer, Reversibility::Trivial)]),
            RiskLevel::Low
        );
        assert_eq!(
            risk_level(&[task(Role::Developer, Reversibility::Hard)]),
            RiskLevel::Medium
        );
        assert_eq!(
            risk_level(&[task(Role::Developer, Reversibility::Irreversible)]),
            RiskLevel::High
        );
    }

    #[test]
    fn wall_time_scales_with_waves() {
        let plan: Vec<PlannedTask> = (0..8)
            .map(|_| task(Role::Developer, Reversibility::Trivial))
            .collect();
        // 8 tasks / 4 concurrency = 2 waves → 4–16 min.
        let est = estimate_plan(&plan, &CostSamples::default(), 4);
        assert_eq!(est.wall_min_low, 4.0);
        assert_eq!(est.wall_min_high, 16.0);
    }

    #[test]
    fn upper_bound_gate_is_strict() {
        let plan = vec![task(Role::Developer, Reversibility::Trivial)];
        let est = estimate_plan(&plan, &CostSamples::default(), 4);
        // high ≈ 0.075; under $1 cap, not under $0.05.
        assert!(est.upper_bound_under(1.00));
        assert!(!est.upper_bound_under(0.05));
    }

    fn run_with_costs(run_id: &str, tasks: &[(Role, f64)]) -> RunState {
        let mut s = RunState::new(run_id, "g", "abc", "b");
        for (i, (role, usd)) in tasks.iter().enumerate() {
            let mut t = crate::model::Task::new(format!("t{i}"), role.clone(), "x");
            t.usd = *usd;
            s.tasks.push(t);
        }
        s
    }

    #[test]
    fn cost_samples_from_runs_buckets_by_role() {
        let runs = vec![
            run_with_costs("r1", &[(Role::Developer, 0.08), (Role::Tester, 0.02)]),
            run_with_costs("r2", &[(Role::Developer, 0.12)]),
        ];
        let samples = cost_samples_from_runs(&runs);
        assert_eq!(samples.per_role["developer"], vec![0.08, 0.12]);
        assert_eq!(samples.per_role["tester"], vec![0.02]);
        assert_eq!(samples.total(), 3);
    }

    #[test]
    fn cost_samples_skips_zero_cost_tasks() {
        // A task that never ran (or whose cost wasn't recorded) is not a
        // sample — it would bias the band toward zero.
        let runs = vec![run_with_costs(
            "r1",
            &[(Role::Developer, 0.0), (Role::Developer, 0.10)],
        )];
        let samples = cost_samples_from_runs(&runs);
        assert_eq!(samples.per_role["developer"], vec![0.10]);
    }

    #[test]
    fn history_from_runs_feeds_the_estimator() {
        // Four developer tasks at ~0.10 across past runs → Medium+
        // confidence and a band centred on history, not the 0.05 prior.
        let runs = vec![
            run_with_costs("r1", &[(Role::Developer, 0.09), (Role::Developer, 0.11)]),
            run_with_costs("r2", &[(Role::Developer, 0.10), (Role::Developer, 0.10)]),
        ];
        let samples = cost_samples_from_runs(&runs);
        let plan = vec![task(Role::Developer, Reversibility::Trivial)];
        let est = estimate_plan(&plan, &samples, 4);
        assert_eq!(est.confidence, Confidence::Medium);
        assert!(est.usd_point > 0.08 && est.usd_point < 0.12);
        assert_eq!(est.sample_count, 4);
    }

    #[test]
    fn render_contains_key_fields() {
        let plan = vec![task(Role::Developer, Reversibility::Hard)];
        let banner = estimate_plan(&plan, &CostSamples::default(), 4).render();
        assert!(banner.contains("Estimated:"));
        assert!(banner.contains("risk: medium"));
        assert!(banner.contains("Confidence: low"));
    }
}
