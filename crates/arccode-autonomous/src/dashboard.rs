//! Pilot-mode dashboard renderer.
//!
//! Pure-function rendering: given a [`RunState`] snapshot + a tail of
//! recent [`Event`]s, return a [`DashboardView`] that describes the three
//! panes (Tasks, Agents, Live log) and the top-bar summary.
//!
//! `arccode pilot status` prints a flat ASCII version; `arccode pilot
//! watch` reuses the same shape under crossterm; a future arccode-tui
//! integration would render the same data into ratatui widgets without
//! touching this module.
//!
//! Live updates: the dashboard caller polls
//! `<run-dir>/state.json` (the modtime is the cheap "did anything
//! change?" signal) and re-renders when it advances. That works for both
//! in-process (orchestrator + dashboard in the same `arccode` invocation)
//! and cross-process (background `arccode pilot` + foreground
//! `arccode pilot watch`) layouts — there is no in-process broadcast
//! requirement.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use thiserror::Error;

use crate::model::{Event, RunState, RunStatus, TaskStatus};
use crate::store::StoreError;

#[derive(Debug, Error)]
pub enum DashboardError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("serde_json: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("no run directory found under {0}")]
    NoRun(PathBuf),
}

/// Brief summary of one run on disk. Returned by [`list_runs`] so the TUI
/// can offer a picker when more than one run is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    pub run_id: String,
    pub dir: PathBuf,
    pub status: RunStatus,
    pub goal: String,
    pub done: usize,
    pub total: usize,
}

impl RunSummary {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            RunStatus::Done | RunStatus::Failed | RunStatus::Aborted
        )
    }

    pub fn top_bar(&self) -> String {
        format!(
            "Pilot: {run_id} · {done}/{total} done",
            run_id = self.run_id,
            done = self.done,
            total = self.total,
        )
    }
}

/// Output of [`render_dashboard`] — three panes and a header. The fields
/// are plain strings so any renderer (ratatui Paragraph, crossterm raw
/// print, HTML if you really want) can lay them out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardView {
    pub top_bar: String,
    pub tasks_pane: String,
    pub agents_pane: String,
    pub log_pane: String,
}

impl DashboardView {
    /// Lay out the dashboard as a flat ASCII screen for `arccode pilot
    /// status`. Top bar on line 1, then the three panes stacked, each
    /// labelled.
    pub fn to_ascii(&self) -> String {
        let mut s = String::new();
        s.push_str("┌─ ");
        s.push_str(&self.top_bar);
        s.push_str(" ─┐\n");
        s.push_str("┌─ Tasks ─────────────────────────────────────────────────┐\n");
        for line in self.tasks_pane.lines() {
            s.push_str(&format!("│ {line}\n"));
        }
        s.push_str("├─ Agents ────────────────────────────────────────────────┤\n");
        for line in self.agents_pane.lines() {
            s.push_str(&format!("│ {line}\n"));
        }
        s.push_str("├─ Live log ──────────────────────────────────────────────┤\n");
        for line in self.log_pane.lines() {
            s.push_str(&format!("│ {line}\n"));
        }
        s.push_str("└──────────────────────────────────────────────────────────┘\n");
        s
    }
}

/// Top-level renderer.
///
/// `recent` is a tail of events for the live-log pane. Pass an empty
/// slice if you don't have one; the pane will just be blank.
pub fn render_dashboard(state: &RunState, recent: &[Event]) -> DashboardView {
    let done = state
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Done)
        .count();
    let total = state.tasks.len();

    let top_bar = format!(
        "Pilot: {run_id} · {done}/{total} · {status:?} · ${cost:.2}",
        run_id = state.run_id,
        status = state.status,
        cost = state.totals.usd,
    );

    let tasks_pane = render_tasks_pane(state);
    let agents_pane = render_agents_pane(state);
    let log_pane = render_log_pane(recent);

    DashboardView {
        top_bar,
        tasks_pane,
        agents_pane,
        log_pane,
    }
}

fn render_tasks_pane(state: &RunState) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    for t in &state.tasks {
        let badge = task_status_badge(t.status);
        let deps = if t.deps.is_empty() {
            "".to_string()
        } else {
            format!(" (deps: {})", t.deps.join(","))
        };
        let agent = t
            .agent
            .as_deref()
            .map(|a| format!(" [{a}]"))
            .unwrap_or_default();
        let _ = writeln!(
            s,
            "{badge} {id:>4} [{role}] {title}{deps}{agent}",
            id = t.id,
            role = t.role.as_str(),
            title = truncate(&t.title, 36),
        );
    }
    if s.is_empty() {
        s.push_str("(no tasks)\n");
    }
    s.trim_end().to_string()
}

fn render_agents_pane(state: &RunState) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    for a in &state.agents {
        let badge = agent_status_badge(a.status);
        let task = a
            .current_task
            .as_deref()
            .map(|t| format!("task={t}"))
            .unwrap_or_else(|| "idle".to_string());
        let pid = a
            .pid
            .map(|p| format!(" pid={p}"))
            .unwrap_or_default();
        let _ = writeln!(
            s,
            "{badge} {id} [{role}] {task}{pid}",
            id = a.id,
            role = a.role.as_str(),
        );
    }
    if s.is_empty() {
        s.push_str("(no agents)\n");
    }
    s.trim_end().to_string()
}

fn render_log_pane(recent: &[Event]) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    for ev in recent.iter().rev().take(12).collect::<Vec<_>>().iter().rev() {
        let _ = writeln!(s, "{}", render_log_line(ev));
    }
    if s.is_empty() {
        s.push_str("(no events yet)\n");
    }
    s.trim_end().to_string()
}

fn render_log_line(ev: &Event) -> String {
    let ts = ev.timestamp();
    let short_ts = ts
        .splitn(3, |c| c == 'T' || c == '+' || c == '.')
        .nth(1)
        .unwrap_or(ts);
    let short_ts = &short_ts[..short_ts.len().min(8)];
    match ev {
        Event::RunStart { run_id, .. } => format!("{short_ts}  run.start  {run_id}"),
        Event::TaskCreate { id, title, .. } => {
            format!("{short_ts}  task.create  {id}: {}", truncate(title, 40))
        }
        Event::TaskAssign { id, agent, .. } => {
            format!("{short_ts}  task.assign  {id} → {agent}")
        }
        Event::TaskStatus { id, status, .. } => {
            format!("{short_ts}  task.status  {id} → {status:?}")
        }
        Event::TaskTool { id, agent, tool, .. } => {
            format!("{short_ts}  task.tool    {id} [{agent}] {tool}")
        }
        Event::TaskCommit { id, sha, .. } => {
            format!("{short_ts}  task.commit  {id} {}", &sha[..sha.len().min(8)])
        }
        Event::AgentSpawn { agent, role, .. } => {
            format!("{short_ts}  agent.spawn  {agent} [{}]", role.as_str())
        }
        Event::AgentStatus { agent, status, .. } => {
            format!("{short_ts}  agent.status {agent} → {status:?}")
        }
        Event::AgentUsd { agent, usd, .. } => {
            format!("{short_ts}  agent.usd    {agent} +${usd:.4}")
        }
        Event::RunStatusEv { status, .. } => format!("{short_ts}  run.status   {status:?}"),
        Event::RunMergeStart { branch, .. } => format!("{short_ts}  run.merge.start {branch}"),
        Event::RunMergeTask { id, commit, .. } => {
            format!("{short_ts}  run.merge    {id} → {}", &commit[..commit.len().min(8)])
        }
        Event::RunPr { url, .. } => format!("{short_ts}  run.pr       {url}"),
        Event::RunDone { .. } => format!("{short_ts}  run.done"),
    }
}

fn task_status_badge(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "  ·",
        TaskStatus::Todo => "  ○",
        TaskStatus::InProgress => "  ↻",
        TaskStatus::Review => "  ◇",
        TaskStatus::Done => "  ✓",
        TaskStatus::Failed => "  ✗",
        TaskStatus::Blocked => "  ‼",
    }
}

fn agent_status_badge(status: crate::model::AgentStatus) -> &'static str {
    match status {
        crate::model::AgentStatus::Idle => "  ·",
        crate::model::AgentStatus::InProgress => "  ↻",
        crate::model::AgentStatus::Done => "  ✓",
        crate::model::AgentStatus::Failed => "  ✗",
        crate::model::AgentStatus::Aborted => "  ⊘",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Discover every run on disk under `<project>/.arccode/autonomous/`,
/// most recent first.
pub fn list_runs(project_root: &Path) -> Result<Vec<RunSummary>, DashboardError> {
    let dir = project_root.join(".arccode").join("autonomous");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<(SystemTime, PathBuf)> = Vec::new();
    for e in std::fs::read_dir(&dir)? {
        let Ok(e) = e else { continue };
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = e.path();
        let state_path = path.join("state.json");
        if !state_path.exists() {
            continue;
        }
        let mtime = std::fs::metadata(&state_path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        entries.push((mtime, path));
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0));

    let mut out = Vec::with_capacity(entries.len());
    for (_, path) in entries {
        match read_run_summary(&path) {
            Ok(summary) => out.push(summary),
            Err(e) => tracing::debug!(target: "pilot::dashboard", "skipping {}: {e}", path.display()),
        }
    }
    Ok(out)
}

fn read_run_summary(run_dir: &Path) -> Result<RunSummary, DashboardError> {
    let state = load_state(run_dir)?;
    let done = state
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Done)
        .count();
    Ok(RunSummary {
        run_id: state.run_id,
        dir: run_dir.to_path_buf(),
        status: state.status,
        goal: state.goal,
        done,
        total: state.tasks.len(),
    })
}

/// Load the latest [`RunState`] snapshot.
pub fn load_state(run_dir: &Path) -> Result<RunState, DashboardError> {
    let path = run_dir.join("state.json");
    let body = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&body)?)
}

/// Return the last `n` events from `<run-dir>/tasks.jsonl`. Cheap enough
/// to call on every redraw — the live log pane shows ~12 lines so
/// re-reading the tail is fine.
pub fn tail_events(run_dir: &Path, n: usize) -> Result<Vec<Event>, DashboardError> {
    let path = run_dir.join("tasks.jsonl");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let body = std::fs::read_to_string(&path)?;
    let mut events: Vec<Event> = Vec::new();
    for line in body.lines().rev().take(n).collect::<Vec<_>>().iter().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(ev) = serde_json::from_str::<Event>(trimmed) {
            events.push(ev);
        }
    }
    Ok(events)
}

/// Mtime-based "did the run change?" probe. Used by `arccode pilot
/// watch`'s polling loop; returns the mtime of `state.json` so the caller
/// can detect changes between ticks.
pub fn state_mtime(run_dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(run_dir.join("state.json"))
        .and_then(|m| m.modified())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Acceptance, AgentStatus, Reversibility, Role};

    fn sample_state() -> RunState {
        let mut s = RunState::new(
            "2026-05-27-1430-abc",
            "add dark-mode toggle",
            "deadbeefcafe1234",
            "arccode/auto/2026-05-27-1430-abc",
        );
        s.status = RunStatus::Running;
        s.totals.usd = 0.42;
        let mut t1 = crate::model::Task::new("t1", Role::Developer, "Wire toggle key");
        t1.status = TaskStatus::Done;
        s.tasks.push(t1);
        let mut t2 = crate::model::Task::new("t2", Role::Designer, "Dark palette");
        t2.status = TaskStatus::InProgress;
        t2.deps = vec!["t1".into()];
        t2.agent = Some("agent-0002".into());
        s.tasks.push(t2);
        s.agents.push(crate::model::Agent {
            id: "agent-0002".into(),
            role: Role::Designer,
            current_task: Some("t2".into()),
            pid: Some(12345),
            status: AgentStatus::InProgress,
            session_id: Some("sess-2".into()),
        });
        s
    }

    #[test]
    fn renders_top_bar_with_done_count() {
        let state = sample_state();
        let view = render_dashboard(&state, &[]);
        assert!(view.top_bar.contains("2026-05-27-1430-abc"));
        assert!(view.top_bar.contains("1/2"));
        assert!(view.top_bar.contains("Running"));
        assert!(view.top_bar.contains("$0.42"));
    }

    #[test]
    fn tasks_pane_includes_each_task_with_status_glyph() {
        let view = render_dashboard(&sample_state(), &[]);
        let p = &view.tasks_pane;
        assert!(p.contains("t1"));
        assert!(p.contains("Wire toggle key"));
        assert!(p.contains("✓"), "done task should have ✓ glyph: {p}");
        assert!(p.contains("t2"));
        assert!(p.contains("↻"), "in-progress task should have ↻ glyph: {p}");
        assert!(p.contains("deps: t1"));
        assert!(p.contains("agent-0002"));
    }

    #[test]
    fn agents_pane_lists_active_workers() {
        let view = render_dashboard(&sample_state(), &[]);
        let p = &view.agents_pane;
        assert!(p.contains("agent-0002"));
        assert!(p.contains("designer"));
        assert!(p.contains("task=t2"));
        assert!(p.contains("pid=12345"));
    }

    #[test]
    fn log_pane_renders_events_in_chronological_order() {
        let events = vec![
            Event::TaskCreate {
                t: "2026-05-27T14:30:00Z".into(),
                id: "t1".into(),
                role: Role::Developer,
                title: "Wire toggle key".into(),
                goal: String::new(),
                deps: vec![],
                writes: vec![],
                acceptance: vec![],
                reversibility: Reversibility::default(),
                reversibility_reason: None,
            },
            Event::TaskTool {
                t: "2026-05-27T14:32:11Z".into(),
                id: "t1".into(),
                agent: "agent-0001".into(),
                tool: "edit_file".into(),
                input_hash: None,
                ok: true,
            },
            Event::TaskStatus {
                t: "2026-05-27T14:35:01Z".into(),
                id: "t1".into(),
                status: TaskStatus::Done,
                outcome: None,
            },
        ];
        let view = render_dashboard(&sample_state(), &events);
        let lines: Vec<&str> = view.log_pane.lines().collect();
        assert_eq!(lines.len(), 3, "should show 3 events: {:?}", lines);
        assert!(lines[0].contains("task.create"));
        assert!(lines[1].contains("task.tool"));
        assert!(lines[2].contains("task.status"));
        assert!(lines[2].contains("Done"));
    }

    #[test]
    fn ascii_layout_is_box_drawn() {
        let view = render_dashboard(&sample_state(), &[]);
        let ascii = view.to_ascii();
        assert!(ascii.contains("Pilot:"));
        assert!(ascii.contains("Tasks"));
        assert!(ascii.contains("Agents"));
        assert!(ascii.contains("Live log"));
    }

    #[tokio::test]
    async fn list_runs_finds_state_files_and_orders_by_mtime() {
        use crate::store::RunStore;
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();

        // Two runs, both with state.json.
        let mut s1 = RunStore::create(
            project.join(".arccode/autonomous/run-a"),
            "run-a",
            "goal A",
            "deadbeef",
            "arccode/auto/run-a",
        )
        .await
        .unwrap();
        s1.append(Event::TaskCreate {
            t: RunStore::now(),
            id: "t1".into(),
            role: Role::Developer,
            title: "x".into(),
            goal: String::new(),
            deps: vec![],
            writes: vec![],
            acceptance: vec![],
            reversibility: Reversibility::default(),
            reversibility_reason: None,
        })
        .await
        .unwrap();
        drop(s1);
        // Ensure mtimes differ enough for the sort to be stable across
        // filesystems with low-resolution timestamps.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let _s2 = RunStore::create(
            project.join(".arccode/autonomous/run-b"),
            "run-b",
            "goal B",
            "deadbeef",
            "arccode/auto/run-b",
        )
        .await
        .unwrap();
        drop(_s2);

        let summaries = list_runs(project).unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].run_id, "run-b", "newest first");
        assert_eq!(summaries[1].run_id, "run-a");
        assert_eq!(summaries[1].total, 1);
        assert_eq!(summaries[1].done, 0);
    }

    /// Phase 7 live-update acceptance: when state.json's mtime advances,
    /// state_mtime() reports the new value and a re-render against the
    /// fresh state shows the new content. This is what `arccode pilot
    /// watch`'s polling loop actually does.
    #[tokio::test]
    async fn dashboard_observes_state_changes_via_mtime() {
        use crate::store::RunStore;
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join(".arccode/autonomous/r1");

        let mut store = RunStore::create(&run_dir, "r1", "goal", "abc", "arccode/auto/r1")
            .await
            .unwrap();
        let first_mtime = state_mtime(&run_dir).unwrap();
        let first_state = load_state(&run_dir).unwrap();
        assert_eq!(first_state.tasks.len(), 0);

        // Filesystem mtime resolution on some Windows filesystems is 1s.
        // Wait long enough for the comparison to be reliable.
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

        store
            .append(Event::TaskCreate {
                t: RunStore::now(),
                id: "t1".into(),
                role: Role::Developer,
                title: "do thing".into(),
                goal: String::new(),
                deps: vec![],
                writes: vec![],
                acceptance: vec![Acceptance::Shell {
                    cmd: "cargo check".into(),
                }],
                reversibility: Reversibility::default(),
                reversibility_reason: None,
            })
            .await
            .unwrap();

        let second_mtime = state_mtime(&run_dir).unwrap();
        assert!(
            second_mtime > first_mtime,
            "state.json mtime should advance after an event (first={first_mtime:?}, second={second_mtime:?})"
        );
        let second_state = load_state(&run_dir).unwrap();
        assert_eq!(second_state.tasks.len(), 1);
        assert_eq!(second_state.tasks[0].title, "do thing");

        // Tail of tasks.jsonl shows the new event.
        let recent = tail_events(&run_dir, 4).unwrap();
        assert!(recent
            .iter()
            .any(|e| matches!(e, Event::TaskCreate { id, .. } if id == "t1")));

        let view = render_dashboard(&second_state, &recent);
        assert!(view.tasks_pane.contains("do thing"));
        assert!(view.log_pane.contains("task.create"));
    }
}
