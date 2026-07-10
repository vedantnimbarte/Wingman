//! J5 — proactive status reporting (push, don't poll).
//!
//! The daemon emits updates instead of waiting to be asked: per-run
//! start/mid/complete/failure, a daily standup, and a weekly summary.
//! These are pure renderers over the run state + a lightweight
//! [`RunSummary`]; the [`crate::notify`] layer (R5) decides where each
//! goes and at what severity.

use crate::model::{Event, RunState, RunStatus};

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

/// Per-phase token totals derived from the run's `agent.usd` events.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PhaseTokens {
    pub phase: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Classify an `agent.usd` event's agent id into a phase bucket. Workers
/// carry ids like `agent-0007`; the pipeline records non-worker phases
/// under the synthetic id `phase:<name>` (manager, reviewer, critic).
fn phase_of(agent: &str) -> &str {
    if let Some(p) = agent.strip_prefix("phase:") {
        p
    } else if agent.starts_with("agent-") {
        "worker"
    } else {
        "other"
    }
}

/// Bucket the run's `agent.usd` events into per-phase token totals, sorted
/// by total tokens descending (ties broken by phase name so the output is
/// deterministic). This is the "where did the tokens go?" baseline.
pub fn tokens_by_phase(events: &[Event]) -> Vec<PhaseTokens> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for ev in events {
        if let Event::AgentUsd {
            agent,
            input_tokens,
            output_tokens,
            ..
        } = ev
        {
            let entry = map.entry(phase_of(agent).to_string()).or_default();
            entry.0 += *input_tokens;
            entry.1 += *output_tokens;
        }
    }
    let mut out: Vec<PhaseTokens> = map
        .into_iter()
        .map(|(phase, (i, o))| PhaseTokens {
            phase,
            input_tokens: i,
            output_tokens: o,
        })
        .collect();
    out.sort_by(|a, b| {
        (b.input_tokens + b.output_tokens)
            .cmp(&(a.input_tokens + a.output_tokens))
            .then_with(|| a.phase.cmp(&b.phase))
    });
    out
}

/// Aggregate the run's `agent.usd` events into per-model token totals, so a
/// completed run can be rolled into the global `~/.wingman/usage.json` and
/// show up in `wingman cost` / the `/usage` modal alongside interactive
/// sessions. Keyed by the model string as the pilot recorded it (already in
/// `provider/model` shape). Cache tokens aren't in the event schema, so only
/// fresh input/output are populated.
pub fn tokens_by_model(events: &[Event]) -> std::collections::BTreeMap<String, wingman_core::Usage> {
    let mut map: std::collections::BTreeMap<String, wingman_core::Usage> =
        std::collections::BTreeMap::new();
    for ev in events {
        if let Event::AgentUsd {
            model,
            input_tokens,
            output_tokens,
            ..
        } = ev
        {
            let u = map.entry(model.clone()).or_default();
            u.input_tokens = u.input_tokens.saturating_add(*input_tokens as u32);
            u.output_tokens = u.output_tokens.saturating_add(*output_tokens as u32);
        }
    }
    map
}

/// Render the per-phase token breakdown as a compact multi-line block for
/// the end-of-run log. Returns a single line when nothing was recorded.
pub fn render_token_breakdown(events: &[Event]) -> String {
    let phases = tokens_by_phase(events);
    if phases.is_empty() {
        return "token usage by phase: none recorded".to_string();
    }
    let (ti, to): (u64, u64) = phases
        .iter()
        .fold((0, 0), |(i, o), p| (i + p.input_tokens, o + p.output_tokens));
    let mut out = format!("token usage by phase (total in={ti} out={to}):");
    for p in &phases {
        out.push_str(&format!(
            "\n  {:<9} in={:>8} out={:>8}",
            p.phase, p.input_tokens, p.output_tokens
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Role, Task, TaskStatus};

    fn usd_event(agent: &str, input: u64, output: u64) -> Event {
        Event::AgentUsd {
            t: "t".into(),
            agent: agent.into(),
            model: String::new(),
            input_tokens: input,
            output_tokens: output,
            usd: 0.0,
        }
    }

    #[test]
    fn tokens_by_phase_buckets_workers_and_phases() {
        let events = vec![
            usd_event("agent-0001", 100, 10),
            usd_event("agent-0002", 200, 20),
            usd_event("phase:manager", 50, 5),
            usd_event("phase:reviewer", 30, 3),
        ];
        let phases = tokens_by_phase(&events);
        // Workers aggregate under one bucket and lead by total tokens.
        assert_eq!(phases[0].phase, "worker");
        assert_eq!(phases[0].input_tokens, 300);
        assert_eq!(phases[0].output_tokens, 30);
        let manager = phases.iter().find(|p| p.phase == "manager").unwrap();
        assert_eq!(manager.input_tokens, 50);
        assert!(phases.iter().any(|p| p.phase == "reviewer"));
    }

    #[test]
    fn tokens_by_model_aggregates_across_agents_and_phases() {
        let usd_model = |model: &str, i: u64, o: u64| Event::AgentUsd {
            t: "t".into(),
            agent: "phase:manager".into(),
            model: model.into(),
            input_tokens: i,
            output_tokens: o,
            usd: 0.0,
        };
        let events = vec![
            usd_model("openrouter/deepseek/deepseek-v4-pro", 100, 10),
            usd_model("openrouter/deepseek/deepseek-v4-pro", 50, 5),
            usd_model("deepseek/deepseek-v4-flash", 20, 2),
        ];
        let by_model = tokens_by_model(&events);
        assert_eq!(by_model.len(), 2);
        let pro = &by_model["openrouter/deepseek/deepseek-v4-pro"];
        assert_eq!(pro.input_tokens, 150);
        assert_eq!(pro.output_tokens, 15);
        assert_eq!(by_model["deepseek/deepseek-v4-flash"].input_tokens, 20);
    }

    #[test]
    fn render_token_breakdown_reports_total_and_empty() {
        assert!(render_token_breakdown(&[]).contains("none recorded"));
        let md = render_token_breakdown(&[usd_event("phase:manager", 40, 4)]);
        assert!(md.contains("total in=40 out=4"));
        assert!(md.contains("manager"));
    }

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
