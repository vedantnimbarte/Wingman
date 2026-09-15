//! R3 — handoff packet on escalation.
//!
//! When J15 ([`crate::escalation`]) trips or the E5 retry ladder
//! exhausts, the run can't proceed without a human. Rather than dumping
//! raw logs, we write a single `escalation.md` to the run directory and
//! link it in every notification. It answers the four questions a human
//! actually has on a 2am page: what was the goal, what did the agent try,
//! why is it stuck, and what's the cheapest next step.
//!
//! Pure rendering ([`render`]) + a thin writer ([`write_packet`]) so the
//! markdown is unit-testable without a filesystem.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use wingman_config::PilotTier;

use crate::escalation::EscalationTrigger;
use crate::model::{Event, RunState, Task};

/// One rung of the E5 retry ladder, as it played out for the blocked task.
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptRecord {
    /// Retry-ladder rung (0 = first attempt, 1 = retry, 2 = escalated model).
    /// Rung 3 splits the task rather than running it again.
    pub rung: u32,
    /// Model the attempt ran on.
    pub model: String,
    /// One-line description of what the attempt did.
    pub summary: String,
    /// What went wrong (acceptance failure, check error, etc.).
    pub outcome: String,
}

/// The attempt history of `task_id`, oldest first, from the `task.attempt`
/// events the worker supervisor records as each E5 ladder attempt ends.
pub fn attempts_for(events: &[Event], task_id: &str) -> Vec<AttemptRecord> {
    events
        .iter()
        .filter_map(|ev| match ev {
            Event::TaskAttempt {
                id,
                rung,
                model,
                status,
                summary,
                tests,
                ..
            } if id == task_id => {
                let mut outcome = format!("{status:?}").to_lowercase();
                if !tests.is_empty() {
                    let passed: u32 = tests.values().sum();
                    outcome.push_str(&format!("; {passed} test(s) passed"));
                }
                Some(AttemptRecord {
                    rung: *rung,
                    model: model.clone().unwrap_or_else(|| "unknown".into()),
                    summary: if summary.is_empty() {
                        "no summary recorded".into()
                    } else {
                        summary.clone()
                    },
                    outcome,
                })
            }
            _ => None,
        })
        .collect()
}

/// Everything the packet renderer needs. Borrows from the live run so the
/// caller doesn't have to clone the whole state.
pub struct HandoffPacket<'a> {
    pub state: &'a RunState,
    pub tier: PilotTier,
    /// The task the run is blocked on, if a single one is identifiable.
    pub blocked_task: Option<&'a Task>,
    /// J15 triggers that fired (may be empty if this is a pure E5
    /// exhaustion).
    pub triggers: &'a [EscalationTrigger],
    /// Retry-ladder attempt history for the blocked task, oldest first.
    pub attempts: &'a [AttemptRecord],
    /// Optional human-readable diagnosis. When `None`, the renderer emits
    /// a generic line pointing at the attempt log.
    pub why_stuck: Option<String>,
    /// Optional concrete next step (file:line + command). When `None`,
    /// suggests resuming after manual inspection.
    pub suggested_next: Option<String>,
}

/// Render the escalation packet as markdown. Mirrors the template in
/// plan.md § R3.
pub fn render(packet: &HandoffPacket<'_>) -> String {
    let s = packet.state;
    let mut out = String::new();

    out.push_str(&format!("# Escalation: {}\n\n", s.run_id));
    out.push_str(&format!("**Goal:** {}\n", s.goal));
    out.push_str(&format!("**Tier:** {}\n", packet.tier));
    let status_line = match packet.blocked_task {
        Some(t) => format!("blocked at task #{} ({})", t.id, t.title),
        None => "blocked".to_string(),
    };
    out.push_str(&format!("**Status:** {status_line}\n"));
    out.push_str(&format!("**Spend:** ${:.2}\n\n", s.totals.usd));

    // Triggers (J15) — only when present.
    if !packet.triggers.is_empty() {
        out.push_str("## Escalation triggers\n\n");
        for trig in packet.triggers {
            out.push_str(&format!(
                "- **{}** — {}\n",
                trig.short_label(),
                trig.render()
            ));
        }
        out.push('\n');
    }

    // Plan summary.
    out.push_str("## Plan\n\n");
    if s.tasks.is_empty() {
        out.push_str("_No tasks recorded._\n\n");
    } else {
        for t in &s.tasks {
            out.push_str(&format!(
                "- #{} [{}] {} — `{:?}`\n",
                t.id,
                t.role.as_str(),
                t.title,
                t.status
            ));
        }
        out.push('\n');
    }

    // What was tried.
    out.push_str("## What was tried\n\n");
    if packet.attempts.is_empty() {
        out.push_str("_No retry-ladder attempts recorded._\n\n");
    } else {
        for a in packet.attempts {
            out.push_str(&format!(
                "- Attempt (rung {}, model `{}`): {} — {}\n",
                a.rung, a.model, a.summary, a.outcome
            ));
        }
        out.push('\n');
    }

    // Why we're stuck.
    out.push_str("## Why we're stuck\n\n");
    match &packet.why_stuck {
        Some(w) => {
            out.push_str(w);
            out.push_str("\n\n");
        }
        None => out.push_str(
            "Automatic diagnosis unavailable. See the attempt log above and the\nper-agent session transcripts under `.wingman/sessions/`.\n\n",
        ),
    }

    // Suggested next step.
    out.push_str("## Suggested next step\n\n");
    match &packet.suggested_next {
        Some(n) => {
            out.push_str(n);
            out.push_str("\n\n");
        }
        None => out
            .push_str("Inspect the blocked task's worktree, resolve the failure, then resume.\n\n"),
    }

    // State / resume footer.
    out.push_str("## State\n\n");
    out.push_str(&format!(
        "Worktrees preserved. Resume with: `wingman pilot resume {}`.\n",
        s.run_id
    ));

    out
}

/// Conventional packet path: `<run_dir>/escalation.md`.
pub fn packet_path(run_dir: &Path) -> PathBuf {
    run_dir.join("escalation.md")
}

/// Render and write the packet to `<run_dir>/escalation.md`, returning the
/// path. Overwrites any prior packet (the latest escalation wins).
pub fn write_packet(run_dir: &Path, packet: &HandoffPacket<'_>) -> io::Result<PathBuf> {
    fs::create_dir_all(run_dir)?;
    let path = packet_path(run_dir);
    fs::write(&path, render(packet))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::escalation::EscalationTrigger;
    use crate::model::{Role, RunState, Task, TaskStatus};

    fn state_with_tasks() -> RunState {
        let mut s = RunState::new(
            "2026-05-29-r1",
            "add dark mode",
            "abc123",
            "wingman/auto/r1",
        );
        s.totals.usd = 0.42;
        let mut t1 = Task::new("t1", Role::Developer, "wire toggle key");
        t1.status = TaskStatus::Done;
        let mut t2 = Task::new("t2", Role::Tester, "smoke test");
        t2.status = TaskStatus::Blocked;
        s.tasks = vec![t1, t2];
        s
    }

    #[test]
    fn render_includes_goal_tier_and_resume_line() {
        let s = state_with_tasks();
        let blocked = s.task("t2");
        let packet = HandoffPacket {
            state: &s,
            tier: PilotTier::Copilot,
            blocked_task: blocked,
            triggers: &[],
            attempts: &[],
            why_stuck: None,
            suggested_next: None,
        };
        let md = render(&packet);
        assert!(md.contains("# Escalation: 2026-05-29-r1"));
        assert!(md.contains("**Goal:** add dark mode"));
        assert!(md.contains("**Tier:** copilot"));
        assert!(md.contains("blocked at task #t2"));
        assert!(md.contains("$0.42"));
        assert!(md.contains("wingman pilot resume 2026-05-29-r1"));
    }

    #[test]
    fn render_lists_triggers_and_attempts() {
        let s = state_with_tasks();
        let triggers = vec![EscalationTrigger::CostHalt {
            spent: 12.0,
            cap: 10.0,
        }];
        let attempts = vec![
            AttemptRecord {
                rung: 1,
                model: "haiku".into(),
                summary: "same worker retry".into(),
                outcome: "cargo test: 2 failures".into(),
            },
            AttemptRecord {
                rung: 2,
                model: "opus".into(),
                summary: "escalated model".into(),
                outcome: "same failures".into(),
            },
        ];
        let packet = HandoffPacket {
            state: &s,
            tier: PilotTier::Copilot,
            blocked_task: s.task("t2"),
            triggers: &triggers,
            attempts: &attempts,
            why_stuck: Some("the mock expects a `kid` claim the new code omits".into()),
            suggested_next: Some("edit token.rs:42 to emit `kid`, re-run tests".into()),
        };
        let md = render(&packet);
        assert!(md.contains("## Escalation triggers"));
        assert!(md.contains("cost halt"));
        assert!(md.contains("rung 1, model `haiku`"));
        assert!(md.contains("rung 2, model `opus`"));
        assert!(md.contains("`kid` claim"));
        assert!(md.contains("token.rs:42"));
    }

    #[test]
    fn render_handles_empty_plan_and_no_attempts() {
        let s = RunState::new("r0", "g", "c", "b");
        let packet = HandoffPacket {
            state: &s,
            tier: PilotTier::Assist,
            blocked_task: None,
            triggers: &[],
            attempts: &[],
            why_stuck: None,
            suggested_next: None,
        };
        let md = render(&packet);
        assert!(md.contains("_No tasks recorded._"));
        assert!(md.contains("_No retry-ladder attempts recorded._"));
        assert!(md.contains("Automatic diagnosis unavailable"));
    }

    #[test]
    fn write_packet_creates_file() {
        let dir = std::env::temp_dir().join(format!("wingman-handoff-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let s = state_with_tasks();
        let packet = HandoffPacket {
            state: &s,
            tier: PilotTier::Copilot,
            blocked_task: s.task("t2"),
            triggers: &[],
            attempts: &[],
            why_stuck: None,
            suggested_next: None,
        };
        let path = write_packet(&dir, &packet).unwrap();
        assert!(path.ends_with("escalation.md"));
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("# Escalation:"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// R3 — the packet's "What was tried" comes from the ladder's recorded
    /// attempts, not an empty list.
    #[test]
    fn attempts_come_from_task_attempt_events() {
        let attempt =
            |id: &str, rung: u32, status: TaskStatus, tests: &[(&str, u32)]| Event::TaskAttempt {
                t: "2026-09-14T10:00:00Z".into(),
                id: id.into(),
                agent: "agent-0001".into(),
                rung,
                model: Some(if rung >= 2 { "opus" } else { "haiku" }.into()),
                status,
                summary: format!("attempt {rung}"),
                tests: tests.iter().map(|(l, n)| (l.to_string(), *n)).collect(),
            };
        let events = vec![
            attempt("t2", 0, TaskStatus::Failed, &[]),
            attempt("t1", 0, TaskStatus::Review, &[]),
            attempt("t2", 2, TaskStatus::Failed, &[("shell: cargo test", 118)]),
        ];
        let got = attempts_for(&events, "t2");
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].rung, got[0].model.as_str()), (0, "haiku"));
        assert_eq!(got[0].outcome, "failed");
        assert_eq!((got[1].rung, got[1].model.as_str()), (2, "opus"));
        assert_eq!(got[1].outcome, "failed; 118 test(s) passed");

        let s = state_with_tasks();
        let md = render(&HandoffPacket {
            state: &s,
            tier: PilotTier::Copilot,
            blocked_task: s.task("t2"),
            triggers: &[],
            attempts: &got,
            why_stuck: None,
            suggested_next: None,
        });
        assert!(md.contains("rung 2, model `opus`): attempt 2"), "{md}");
        assert!(!md.contains("No retry-ladder attempts recorded"));
    }
}
