//! J5 — proactive status reporting (push, don't poll).
//!
//! The daemon emits updates instead of waiting to be asked: per-run
//! start/mid/complete/failure, a daily standup, and a weekly summary.
//! These are pure renderers over the run state + a lightweight
//! [`RunSummary`]; the [`crate::notify`] layer (R5) decides where each
//! goes and at what severity.

use crate::model::{RunState, RunStatus};

/// One run's headline facts, used for standup / weekly rollups.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    pub run_id: String,
    pub goal: String,
    pub status: RunStatus,
    pub usd: f64,
    pub pr_url: Option<String>,
}

impl RunSummary {
    pub fn from_state(state: &RunState) -> Self {
        Self {
            run_id: state.run_id.clone(),
            goal: state.goal.clone(),
            status: state.status,
            usd: state.totals.usd,
            pr_url: state.pr_url.clone(),
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let mut t: String = line.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// Per-run start notice.
pub fn render_run_start(state: &RunState) -> String {
    format!(
        "▶ pilot run `{}` started: {} ({} task(s))",
        state.run_id,
        truncate(&state.goal, 60),
        state.tasks.len()
    )
}

/// Mid-run notice — only meaningful when spend crosses 50% of the
/// estimate. Returns `None` below the threshold so the caller doesn't spam.
pub fn render_run_progress(state: &RunState, estimated_usd: f64) -> Option<String> {
    if estimated_usd <= 0.0 {
        return None;
    }
    let frac = state.totals.usd / estimated_usd;
    if frac < 0.5 {
        return None;
    }
    let done = state
        .tasks
        .iter()
        .filter(|t| t.status == crate::model::TaskStatus::Done)
        .count();
    Some(format!(
        "⏳ run `{}`: {}/{} tasks done, ${:.2} spent ({:.0}% of ${:.2} est.)",
        state.run_id,
        done,
        state.tasks.len(),
        state.totals.usd,
        frac * 100.0,
        estimated_usd,
    ))
}

/// Completion notice.
pub fn render_run_complete(state: &RunState) -> String {
    let pr = state
        .pr_url
        .as_deref()
        .map(|u| format!(" — {u}"))
        .unwrap_or_default();
    format!(
        "✅ run `{}` complete: {} task(s), ${:.2}{pr}",
        state.run_id,
        state.tasks.len(),
        state.totals.usd,
    )
}

/// Failure notice.
pub fn render_run_failure(state: &RunState, reason: &str) -> String {
    format!(
        "❌ run `{}` failed: {reason} (${:.2} spent)",
        state.run_id, state.totals.usd
    )
}

/// Daily standup over the previous day's runs.
pub fn render_standup(runs: &[RunSummary]) -> String {
    if runs.is_empty() {
        return "📋 Standup: no pilot runs in the last day.".to_string();
    }
    let merged = runs.iter().filter(|r| r.pr_url.is_some()).count();
    let failed = runs
        .iter()
        .filter(|r| r.status == RunStatus::Failed)
        .count();
    let total_usd: f64 = runs.iter().map(|r| r.usd).sum();
    let mut out = format!(
        "📋 Standup: {} run(s), {merged} with PRs, {failed} failed, ${total_usd:.2} spent.\n",
        runs.len()
    );
    for r in runs {
        out.push_str(&format!(
            "- `{}` [{:?}] {}\n",
            r.run_id,
            r.status,
            truncate(&r.goal, 50)
        ));
    }
    out
}

/// Weekly summary with simple trend hints.
pub fn render_weekly_summary(runs: &[RunSummary]) -> String {
    if runs.is_empty() {
        return "📊 Weekly summary: no pilot activity this week.".to_string();
    }
    let n = runs.len();
    let merged = runs.iter().filter(|r| r.pr_url.is_some()).count();
    let failed = runs
        .iter()
        .filter(|r| r.status == RunStatus::Failed)
        .count();
    let total_usd: f64 = runs.iter().map(|r| r.usd).sum();
    let success_rate = (n - failed) as f64 / n as f64 * 100.0;
    let mut out = format!(
        "📊 Weekly: {n} run(s), {success_rate:.0}% reached terminal-success, {merged} PRs, ${total_usd:.2} spent.\n"
    );
    if failed as f64 / n as f64 > 0.3 {
        out.push_str("- ⚠️ failure rate >30% — consider tightening the plan-approval gate or the goal scope.\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Role, Task, TaskStatus};

    fn state() -> RunState {
        let mut s = RunState::new("r1", "add dark mode toggle to the TUI", "abc", "b");
        s.tasks.push(Task::new("t1", Role::Developer, "x"));
        s.tasks.push(Task::new("t2", Role::Tester, "y"));
        s
    }

    #[test]
    fn run_start_lists_task_count() {
        let md = render_run_start(&state());
        assert!(md.contains("started"));
        assert!(md.contains("2 task"));
    }

    #[test]
    fn progress_suppressed_below_half_estimate() {
        let mut s = state();
        s.totals.usd = 0.2;
        assert!(render_run_progress(&s, 1.0).is_none());
    }

    #[test]
    fn progress_fires_at_half_estimate() {
        let mut s = state();
        s.totals.usd = 0.6;
        s.tasks[0].status = TaskStatus::Done;
        let md = render_run_progress(&s, 1.0).unwrap();
        assert!(md.contains("1/2 tasks done"));
        assert!(md.contains("60%"));
    }

    #[test]
    fn progress_none_without_estimate() {
        assert!(render_run_progress(&state(), 0.0).is_none());
    }

    #[test]
    fn complete_includes_pr_url_when_present() {
        let mut s = state();
        s.pr_url = Some("https://github.com/x/y/pull/1".into());
        s.totals.usd = 0.42;
        let md = render_run_complete(&s);
        assert!(md.contains("complete"));
        assert!(md.contains("pull/1"));
        assert!(md.contains("$0.42"));
    }

    #[test]
    fn failure_includes_reason() {
        let md = render_run_failure(&state(), "cost cap breached");
        assert!(md.contains("failed"));
        assert!(md.contains("cost cap breached"));
    }

    #[test]
    fn standup_empty() {
        assert!(render_standup(&[]).contains("no pilot runs"));
    }

    #[test]
    fn standup_counts_merges_and_failures() {
        let runs = vec![
            RunSummary {
                run_id: "r1".into(),
                goal: "a".into(),
                status: RunStatus::Done,
                usd: 0.1,
                pr_url: Some("u".into()),
            },
            RunSummary {
                run_id: "r2".into(),
                goal: "b".into(),
                status: RunStatus::Failed,
                usd: 0.2,
                pr_url: None,
            },
        ];
        let md = render_standup(&runs);
        assert!(md.contains("2 run(s)"));
        assert!(md.contains("1 with PRs"));
        assert!(md.contains("1 failed"));
        assert!(md.contains("$0.30"));
    }

    #[test]
    fn weekly_flags_high_failure_rate() {
        let runs: Vec<RunSummary> = (0..10)
            .map(|i| RunSummary {
                run_id: format!("r{i}"),
                goal: "g".into(),
                status: if i < 4 {
                    RunStatus::Failed
                } else {
                    RunStatus::Done
                },
                usd: 0.1,
                pr_url: None,
            })
            .collect();
        let md = render_weekly_summary(&runs);
        assert!(md.contains("failure rate >30%"));
    }

    #[test]
    fn weekly_empty() {
        assert!(render_weekly_summary(&[]).contains("no pilot activity"));
    }
}
