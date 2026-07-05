//! R4 — eval / regression harness (scoring core).
//!
//! A nightly run of 20–30 canned goals against a frozen repo snapshot,
//! scored on four axes: success rate, cost, wall time, and an LLM-judge
//! quality score. CI gates any commit touching planner prompts, role
//! markdown, the tool registry, or orchestrator behaviour: if any axis
//! regresses more than a threshold (default 10%) vs the baseline, the PR
//! fails unless explicitly overridden.
//!
//! This module is the scoring + comparison core. Running the goals and
//! the LLM-judge are the orchestrator/harness's job; here we summarise
//! results and detect regressions. Axis direction matters: success rate
//! and quality are "higher is better"; cost and wall time are "lower is
//! better".

use serde::{Deserialize, Serialize};

/// One canned-goal result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalResult {
    pub goal: String,
    /// Did the run produce a CI-green PR?
    pub success: bool,
    pub usd: f64,
    pub wall_min: f64,
    /// LLM-judge diff-quality score in `[0,1]` vs a golden diff.
    pub quality: f64,
}

/// Aggregate across a suite of results.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalSummary {
    pub n: usize,
    pub success_rate: f64,
    pub avg_usd: f64,
    pub avg_wall: f64,
    pub avg_quality: f64,
}

pub fn summarize(results: &[EvalResult]) -> EvalSummary {
    let n = results.len();
    if n == 0 {
        return EvalSummary {
            n: 0,
            success_rate: 0.0,
            avg_usd: 0.0,
            avg_wall: 0.0,
            avg_quality: 0.0,
        };
    }
    let nf = n as f64;
    let successes = results.iter().filter(|r| r.success).count() as f64;
    EvalSummary {
        n,
        success_rate: successes / nf,
        avg_usd: results.iter().map(|r| r.usd).sum::<f64>() / nf,
        avg_wall: results.iter().map(|r| r.wall_min).sum::<f64>() / nf,
        avg_quality: results.iter().map(|r| r.quality).sum::<f64>() / nf,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// Higher is better (success rate, quality).
    HigherBetter,
    /// Lower is better (cost, wall time).
    LowerBetter,
}

/// Per-axis comparison vs baseline.
#[derive(Debug, Clone, PartialEq)]
pub struct AxisDelta {
    pub name: String,
    pub baseline: f64,
    pub current: f64,
    /// Signed fractional change `(current - baseline) / baseline`.
    pub pct_change: f64,
    /// True when this axis regressed beyond the threshold.
    pub regressed: bool,
}

/// Full regression report.
#[derive(Debug, Clone, PartialEq)]
pub struct RegressionReport {
    pub axes: Vec<AxisDelta>,
    /// True when any axis regressed.
    pub regressed: bool,
}

fn axis(name: &str, baseline: f64, current: f64, dir: Direction, threshold: f64) -> AxisDelta {
    // pct change relative to baseline; if baseline is 0 we can't form a
    // ratio, so treat any worsening as a flat regression and improvement
    // as fine.
    let pct_change = if baseline == 0.0 {
        if current == 0.0 {
            0.0
        } else {
            // From-zero change is undefined as a ratio; report the raw
            // delta sign via ±infinity-ish sentinel kept finite.
            match dir {
                Direction::HigherBetter => 1.0, // current>0 is an improvement
                Direction::LowerBetter => 1.0,  // current>0 is a worsening
            }
        }
    } else {
        (current - baseline) / baseline
    };

    let regressed = match dir {
        Direction::HigherBetter => {
            // Worse means current dropped below baseline by > threshold.
            baseline > 0.0 && current < baseline * (1.0 - threshold)
        }
        Direction::LowerBetter => {
            if baseline == 0.0 {
                current > 0.0
            } else {
                current > baseline * (1.0 + threshold)
            }
        }
    };

    AxisDelta {
        name: name.to_string(),
        baseline,
        current,
        pct_change,
        regressed,
    }
}

/// Compare a current summary to a baseline. `threshold` is the allowed
/// fractional drift (0.10 = 10%).
pub fn compare(current: &EvalSummary, baseline: &EvalSummary, threshold: f64) -> RegressionReport {
    let axes = vec![
        axis(
            "success_rate",
            baseline.success_rate,
            current.success_rate,
            Direction::HigherBetter,
            threshold,
        ),
        axis(
            "avg_usd",
            baseline.avg_usd,
            current.avg_usd,
            Direction::LowerBetter,
            threshold,
        ),
        axis(
            "avg_wall",
            baseline.avg_wall,
            current.avg_wall,
            Direction::LowerBetter,
            threshold,
        ),
        axis(
            "avg_quality",
            baseline.avg_quality,
            current.avg_quality,
            Direction::HigherBetter,
            threshold,
        ),
    ];
    let regressed = axes.iter().any(|a| a.regressed);
    RegressionReport { axes, regressed }
}

/// Render a markdown dashboard row-set for `.wingman/eval/`.
pub fn render_report(report: &RegressionReport) -> String {
    let mut out = String::from("# Eval regression report\n\n");
    out.push_str(&format!(
        "**Overall: {}**\n\n",
        if report.regressed {
            "⛔ REGRESSED"
        } else {
            "✅ within tolerance"
        }
    ));
    out.push_str("| Axis | Baseline | Current | Δ | Status |\n");
    out.push_str("| ---- | -------- | ------- | - | ------ |\n");
    for a in &report.axes {
        out.push_str(&format!(
            "| {} | {:.3} | {:.3} | {:+.1}% | {} |\n",
            a.name,
            a.baseline,
            a.current,
            a.pct_change * 100.0,
            if a.regressed { "⛔" } else { "ok" }
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(success: bool, usd: f64, wall: f64, quality: f64) -> EvalResult {
        EvalResult {
            goal: "g".into(),
            success,
            usd,
            wall_min: wall,
            quality,
        }
    }

    #[test]
    fn summarize_averages() {
        let results = vec![result(true, 0.10, 5.0, 0.8), result(false, 0.30, 15.0, 0.6)];
        let s = summarize(&results);
        assert_eq!(s.n, 2);
        assert_eq!(s.success_rate, 0.5);
        assert!((s.avg_usd - 0.20).abs() < 1e-9);
        assert!((s.avg_wall - 10.0).abs() < 1e-9);
        assert!((s.avg_quality - 0.7).abs() < 1e-9);
    }

    #[test]
    fn summarize_empty() {
        let s = summarize(&[]);
        assert_eq!(s.n, 0);
        assert_eq!(s.success_rate, 0.0);
    }

    #[test]
    fn no_change_is_not_a_regression() {
        let base = summarize(&[result(true, 0.1, 5.0, 0.9)]);
        let cur = base.clone();
        let r = compare(&cur, &base, 0.10);
        assert!(!r.regressed);
    }

    #[test]
    fn cost_increase_beyond_threshold_regresses() {
        let base = EvalSummary {
            n: 1,
            success_rate: 1.0,
            avg_usd: 0.10,
            avg_wall: 5.0,
            avg_quality: 0.9,
        };
        let cur = EvalSummary {
            avg_usd: 0.12,
            ..base.clone()
        }; // +20% > 10%
        let r = compare(&cur, &base, 0.10);
        assert!(r.regressed);
        assert!(
            r.axes
                .iter()
                .find(|a| a.name == "avg_usd")
                .unwrap()
                .regressed
        );
    }

    #[test]
    fn cost_increase_within_threshold_ok() {
        let base = EvalSummary {
            n: 1,
            success_rate: 1.0,
            avg_usd: 0.10,
            avg_wall: 5.0,
            avg_quality: 0.9,
        };
        let cur = EvalSummary {
            avg_usd: 0.105,
            ..base.clone()
        }; // +5%
        assert!(!compare(&cur, &base, 0.10).regressed);
    }

    #[test]
    fn success_rate_drop_regresses() {
        let base = EvalSummary {
            n: 10,
            success_rate: 0.90,
            avg_usd: 0.1,
            avg_wall: 5.0,
            avg_quality: 0.9,
        };
        let cur = EvalSummary {
            success_rate: 0.70,
            ..base.clone()
        }; // big drop
        assert!(compare(&cur, &base, 0.10).regressed);
    }

    #[test]
    fn improvements_never_regress() {
        let base = EvalSummary {
            n: 10,
            success_rate: 0.8,
            avg_usd: 0.2,
            avg_wall: 10.0,
            avg_quality: 0.7,
        };
        // Cheaper, faster, higher success + quality.
        let cur = EvalSummary {
            n: 10,
            success_rate: 0.95,
            avg_usd: 0.1,
            avg_wall: 5.0,
            avg_quality: 0.9,
        };
        let r = compare(&cur, &base, 0.10);
        assert!(!r.regressed);
    }

    #[test]
    fn render_report_marks_regression() {
        let base = EvalSummary {
            n: 1,
            success_rate: 1.0,
            avg_usd: 0.10,
            avg_wall: 5.0,
            avg_quality: 0.9,
        };
        let cur = EvalSummary {
            avg_wall: 20.0,
            ..base.clone()
        };
        let md = render_report(&compare(&cur, &base, 0.10));
        assert!(md.contains("REGRESSED"));
        assert!(md.contains("avg_wall"));
    }
}
