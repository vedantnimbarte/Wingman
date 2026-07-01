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

use crate::model::{AgentStatus, Event, RunState, RunStatus, TaskStatus};
use crate::store::StoreError;

use chrono::{DateTime, Utc};

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

/// Coarse severity of a live-log line, so a colour-capable renderer (the
/// ratatui `pilot watch` UI) can tint events without re-parsing the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSeverity {
    Info,
    Ok,
    Warn,
    Error,
}

/// One row in the live-log pane: the formatted line plus its severity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    pub text: String,
    pub severity: LogSeverity,
}

/// One row in the tasks pane — a structured view of a [`crate::model::Task`]
/// with the derived fields (elapsed, write count) the dashboard surfaces.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskRow {
    pub id: String,
    pub role: String,
    pub title: String,
    pub status: TaskStatus,
    pub deps: Vec<String>,
    pub writes: usize,
    pub usd: f64,
    pub elapsed_secs: Option<i64>,
    pub attempts: u32,
    pub agent: Option<String>,
}

/// One row in the agents pane.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRow {
    pub id: String,
    pub role: String,
    pub status: AgentStatus,
    pub task: Option<String>,
    pub pid: Option<u32>,
    pub tool: Option<String>,
    pub uptime_secs: Option<i64>,
    pub usd: f64,
}

/// Enriched run header: progress counts by status, spend, elapsed, and the
/// git anchors (integration branch + base commit).
#[derive(Debug, Clone, PartialEq)]
pub struct HeaderInfo {
    pub run_id: String,
    pub status: RunStatus,
    pub done: usize,
    pub running: usize,
    pub failed: usize,
    pub blocked: usize,
    pub total: usize,
    pub usd: f64,
    pub elapsed_secs: Option<i64>,
    pub branch: String,
    pub base_short: String,
}

/// Structured dashboard snapshot. This is the source of truth both the
/// plain-ASCII `to_ascii` grid and the ratatui `pilot watch` UI render
/// from — keeping the two views consistent.
#[derive(Debug, Clone, PartialEq)]
pub struct DashboardModel {
    pub header: HeaderInfo,
    pub tasks: Vec<TaskRow>,
    pub agents: Vec<AgentRow>,
    pub log: Vec<LogRow>,
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

/// Default total width of the ASCII grid. Chosen to fit a standard 100-col
/// terminal; the two top boxes split it, the log spans it.
const GRID_WIDTH: usize = 100;

impl DashboardView {
    /// Lay out the dashboard as a 2-column ASCII grid for `arccode pilot
    /// status`: a header line, then Tasks | Agents side by side, then the
    /// Live log spanning the full width underneath.
    pub fn to_ascii(&self) -> String {
        self.to_ascii_width(GRID_WIDTH)
    }

    /// Grid layout at an explicit total width (min 40). Splitting it out
    /// lets a width-aware caller (or a test) pin the geometry.
    pub fn to_ascii_width(&self, total: usize) -> String {
        let total = total.max(40);
        // Left (Tasks) gets a bit more room than right (Agents) — task
        // titles are the long strings.
        let left_w = (total * 56 / 100).max(24);
        let right_w = total - left_w;

        let left = boxed_lines("Tasks", self.tasks_pane.lines(), left_w);
        let right = boxed_lines("Agents", self.agents_pane.lines(), right_w);

        let mut s = String::new();
        s.push_str(&header_bar(&self.top_bar, total));
        s.push('\n');
        // Zip the two boxes row-by-row, padding the shorter one with blanks.
        let rows = left.len().max(right.len());
        let left_blank = " ".repeat(left_w);
        let right_blank = " ".repeat(right_w);
        for i in 0..rows {
            let l = left.get(i).map(String::as_str).unwrap_or(&left_blank);
            let r = right.get(i).map(String::as_str).unwrap_or(&right_blank);
            s.push_str(l);
            s.push_str(r);
            s.push('\n');
        }
        for line in boxed_lines("Live log", self.log_pane.lines(), total) {
            s.push_str(&line);
            s.push('\n');
        }
        s
    }
}

/// Render a single-line header bar `┌ <text> ───┐` padded to `width`.
fn header_bar(text: &str, width: usize) -> String {
    let width = width.max(6);
    // "┌ " + text + " " + fill + "┐"
    let used = 2 + text.chars().count() + 1;
    let fill = width.saturating_sub(used + 1);
    format!("┌ {text} {}┐", "─".repeat(fill))
}

/// Render `lines` inside a titled box of total width `width`
/// (`┌─ Title ─┐` / `│ … │` / `└─┘`). Each line is truncated / padded to fit.
fn boxed_lines<'a>(title: &str, lines: impl Iterator<Item = &'a str>, width: usize) -> Vec<String> {
    let width = width.max(8);
    // Content width = total − "│ " (2) − " │" (2).
    let cw = width - 4;
    let mut out = Vec::new();

    // Top border with embedded title: "┌─ Title " + fill + "┐"
    let title_used = 3 + title.chars().count() + 1; // "┌─ " + title + " "
    let top_fill = width.saturating_sub(title_used + 1);
    out.push(format!("┌─ {title} {}┐", "─".repeat(top_fill)));

    for line in lines {
        let content = pad_or_truncate(line, cw);
        out.push(format!("│ {content} │"));
    }
    // Bottom border.
    out.push(format!("└{}┘", "─".repeat(width - 2)));
    out
}

/// Pad with spaces or truncate (with an ellipsis) so the visible width is
/// exactly `w` columns.
fn pad_or_truncate(s: &str, w: usize) -> String {
    let len = s.chars().count();
    if len == w {
        s.to_string()
    } else if len < w {
        format!("{s}{}", " ".repeat(w - len))
    } else {
        let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Top-level renderer.
///
/// `recent` is a tail of events for the live-log pane. Pass an empty
/// slice if you don't have one; the pane will just be blank.
pub fn render_dashboard(state: &RunState, recent: &[Event]) -> DashboardView {
    build_model(state, recent, Some(Utc::now())).to_view()
}

/// Build the structured [`DashboardModel`]. `now` is the reference clock for
/// live durations (elapsed / uptime of not-yet-finished work); pass `None`
/// for a deterministic snapshot that only reports durations of finished
/// items (used by tests).
pub fn build_model(state: &RunState, recent: &[Event], now: Option<DateTime<Utc>>) -> DashboardModel {
    let count = |st: TaskStatus| state.tasks.iter().filter(|t| t.status == st).count();
    let run_started = run_started_at(&state.run_id).or_else(|| {
        state
            .tasks
            .iter()
            .filter_map(|t| t.started_at.as_deref().and_then(parse_ts))
            .min()
    });
    let elapsed_secs = match (run_started, now) {
        (Some(start), Some(n)) => Some((n - start).num_seconds().max(0)),
        _ => None,
    };

    let header = HeaderInfo {
        run_id: state.run_id.clone(),
        status: state.status,
        done: count(TaskStatus::Done),
        running: count(TaskStatus::InProgress),
        failed: count(TaskStatus::Failed),
        blocked: count(TaskStatus::Blocked),
        total: state.tasks.len(),
        usd: state.totals.usd,
        elapsed_secs,
        branch: state.integration_branch.clone(),
        base_short: short_sha(&state.base_commit),
    };

    let tasks = state
        .tasks
        .iter()
        .map(|t| TaskRow {
            id: t.id.clone(),
            role: t.role.as_str().to_string(),
            title: t.title.clone(),
            status: t.status,
            deps: t.deps.clone(),
            writes: t.writes.len(),
            usd: t.usd,
            elapsed_secs: span_secs(t.started_at.as_deref(), t.ended_at.as_deref(), now),
            attempts: t.attempts,
            agent: t.agent.clone(),
        })
        .collect();

    let agents = state
        .agents
        .iter()
        .map(|a| AgentRow {
            id: a.id.clone(),
            role: a.role.as_str().to_string(),
            status: a.status,
            task: a.current_task.clone(),
            pid: a.pid,
            tool: a.current_tool.clone(),
            // Uptime runs until the worker finishes; for terminal agents we
            // still show spawn→now since we don't record a stop stamp.
            uptime_secs: span_secs(a.spawned_at.as_deref(), None, now),
            usd: a.usd,
        })
        .collect();

    // `recent` is already the caller-chosen tail, in chronological order.
    let log = recent.iter().map(render_log_line).collect();

    DashboardModel {
        header,
        tasks,
        agents,
        log,
    }
}

impl DashboardModel {
    /// Flatten the model into the string-pane [`DashboardView`] used by the
    /// ASCII grid.
    pub fn to_view(&self) -> DashboardView {
        DashboardView {
            top_bar: self.header.top_bar(),
            tasks_pane: join_or(self.tasks.iter().map(task_row_line), "(no tasks)"),
            agents_pane: join_or(self.agents.iter().map(agent_row_line), "(no agents)"),
            log_pane: join_or(self.log.iter().map(|r| r.text.clone()), "(no events yet)"),
        }
    }
}

impl HeaderInfo {
    /// The one-line summary shown in the top bar.
    pub fn top_bar(&self) -> String {
        let mut s = format!(
            "Pilot: {} · {}/{} · {:?} · ${:.2}",
            self.run_id, self.done, self.total, self.status, self.usd,
        );
        // Only surface the noteworthy non-done counts to keep it compact.
        if self.running > 0 {
            s.push_str(&format!(" · {}▶", self.running));
        }
        if self.failed > 0 {
            s.push_str(&format!(" · {}✗", self.failed));
        }
        if self.blocked > 0 {
            s.push_str(&format!(" · {}‼", self.blocked));
        }
        if let Some(secs) = self.elapsed_secs {
            s.push_str(&format!(" · {}", fmt_dur(secs)));
        }
        s
    }
}

/// Compact single-line rendering of a task row for the ASCII pane. Primary
/// identity up front, then a `·`-separated tail of the details worth
/// surfacing (agent, deps, writes, cost, elapsed, retries).
fn task_row_line(t: &TaskRow) -> String {
    let badge = task_status_badge(t.status);
    let mut meta: Vec<String> = Vec::new();
    if let Some(a) = &t.agent {
        meta.push(a.clone());
    }
    if !t.deps.is_empty() {
        meta.push(format!("deps: {}", t.deps.join(",")));
    }
    if t.writes > 0 {
        meta.push(format!("w{}", t.writes));
    }
    if t.usd > 0.0 {
        meta.push(format!("${:.2}", t.usd));
    }
    if let Some(secs) = t.elapsed_secs {
        meta.push(fmt_dur(secs));
    }
    if t.attempts > 1 {
        meta.push(format!("try{}", t.attempts));
    }
    let tail = if meta.is_empty() {
        String::new()
    } else {
        format!("  · {}", meta.join(" · "))
    };
    format!(
        "{badge} {id} [{role}] {title}{tail}",
        id = t.id,
        role = t.role,
        title = t.title,
    )
}

/// Compact single-line rendering of an agent row.
fn agent_row_line(a: &AgentRow) -> String {
    let badge = agent_status_badge(a.status);
    let mut meta: Vec<String> = Vec::new();
    meta.push(
        a.task
            .as_deref()
            .map(|t| format!("task={t}"))
            .unwrap_or_else(|| "idle".to_string()),
    );
    if let Some(tool) = &a.tool {
        meta.push(format!("▸{tool}"));
    }
    if let Some(p) = a.pid {
        meta.push(format!("pid={p}"));
    }
    if let Some(secs) = a.uptime_secs {
        meta.push(fmt_dur(secs));
    }
    if a.usd > 0.0 {
        meta.push(format!("${:.2}", a.usd));
    }
    format!(
        "{badge} {id} [{role}] {meta}",
        id = a.id,
        role = a.role,
        meta = meta.join(" · "),
    )
}

/// Join an iterator of lines with newlines, or fall back to `empty` when
/// there are none.
fn join_or(lines: impl Iterator<Item = String>, empty: &str) -> String {
    let joined = lines.collect::<Vec<_>>().join("\n");
    if joined.is_empty() {
        empty.to_string()
    } else {
        joined
    }
}

fn render_log_line(ev: &Event) -> LogRow {
    use LogSeverity::*;
    let ts = ev.timestamp();
    let short_ts = ts.split(['T', '+', '.']).nth(1).unwrap_or(ts);
    let short_ts = &short_ts[..short_ts.len().min(8)];
    let (severity, text) = match ev {
        Event::RunStart { run_id, .. } => (Info, format!("{short_ts}  run.start  {run_id}")),
        Event::TaskCreate { id, title, .. } => (
            Info,
            format!("{short_ts}  task.create  {id}: {}", truncate(title, 40)),
        ),
        Event::TaskAssign { id, agent, .. } => {
            (Info, format!("{short_ts}  task.assign  {id} → {agent}"))
        }
        Event::TaskStatus { id, status, .. } => {
            let sev = match status {
                TaskStatus::Done => Ok,
                TaskStatus::Failed => Error,
                TaskStatus::Blocked => Warn,
                _ => Info,
            };
            (sev, format!("{short_ts}  task.status  {id} → {status:?}"))
        }
        Event::TaskTool {
            id, agent, tool, ok, ..
        } => {
            let mark = if *ok { "" } else { " ✗" };
            let sev = if *ok { Info } else { Error };
            (
                sev,
                format!("{short_ts}  task.tool    {id} [{agent}] {tool}{mark}"),
            )
        }
        Event::TaskCommit { id, sha, .. } => (
            Ok,
            format!("{short_ts}  task.commit  {id} {}", &sha[..sha.len().min(8)]),
        ),
        Event::AgentSpawn { agent, role, .. } => (
            Info,
            format!("{short_ts}  agent.spawn  {agent} [{}]", role.as_str()),
        ),
        Event::AgentStatus { agent, status, .. } => {
            let sev = match status {
                AgentStatus::Done => Ok,
                AgentStatus::Failed => Error,
                AgentStatus::Aborted => Warn,
                _ => Info,
            };
            (sev, format!("{short_ts}  agent.status {agent} → {status:?}"))
        }
        Event::AgentUsd { agent, usd, .. } => {
            (Info, format!("{short_ts}  agent.usd    {agent} +${usd:.4}"))
        }
        Event::RunStatusEv { status, .. } => {
            let sev = match status {
                RunStatus::Failed | RunStatus::Aborted => Error,
                RunStatus::Done => Ok,
                _ => Info,
            };
            (sev, format!("{short_ts}  run.status   {status:?}"))
        }
        Event::RunMergeStart { branch, .. } => {
            (Info, format!("{short_ts}  run.merge.start {branch}"))
        }
        Event::RunMergeTask { id, commit, .. } => (
            Ok,
            format!(
                "{short_ts}  run.merge    {id} → {}",
                &commit[..commit.len().min(8)]
            ),
        ),
        Event::RunPr { url, .. } => (Ok, format!("{short_ts}  run.pr       {url}")),
        Event::RunDone { .. } => (Ok, format!("{short_ts}  run.done")),
        Event::PrOutcome { kind, .. } => {
            (Info, format!("{short_ts}  pr.outcome   {}", kind.as_str()))
        }
    };
    LogRow { text, severity }
}

/// First 8 chars of a git sha (or the whole thing if shorter).
fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// Parse an RFC-3339 timestamp into UTC; `None` on any parse failure.
fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Seconds spanned by `start`→`end`. If `end` is absent, run to `now`
/// (a live, still-ticking span). `None` when we can't compute it.
fn span_secs(start: Option<&str>, end: Option<&str>, now: Option<DateTime<Utc>>) -> Option<i64> {
    let start = parse_ts(start?)?;
    let end = match end {
        Some(e) => parse_ts(e)?,
        None => now?,
    };
    Some((end - start).num_seconds().max(0))
}

/// Best-effort run start time parsed from the `YYYY-MM-DD-HHMM-<rand>` run
/// id. `None` if the id doesn't match that shape.
fn run_started_at(run_id: &str) -> Option<DateTime<Utc>> {
    let parts: Vec<&str> = run_id.split('-').collect();
    if parts.len() < 4 {
        return None;
    }
    let (y, m, d, hm) = (parts[0], parts[1], parts[2], parts[3]);
    if hm.len() != 4 {
        return None;
    }
    let iso = format!("{y}-{m}-{d}T{}:{}:00Z", &hm[..2], &hm[2..4]);
    parse_ts(&iso)
}

/// Format a duration in seconds compactly: `45s`, `1m20s`, `2h03m`.
fn fmt_dur(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
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
    entries.sort_by_key(|e| std::cmp::Reverse(e.0));

    let mut out = Vec::with_capacity(entries.len());
    for (_, path) in entries {
        match read_run_summary(&path) {
            Ok(summary) => out.push(summary),
            Err(e) => {
                tracing::debug!(target: "pilot::dashboard", "skipping {}: {e}", path.display())
            }
        }
    }
    Ok(out)
}

/// Load the full [`RunState`] snapshot of every run on disk under
/// `<project>/.arccode/autonomous/`. Corrupt or unreadable snapshots are
/// skipped (best-effort), so callers can use this for history-derived
/// signals (e.g. J9 cost samples) without a single bad run wedging them.
pub fn load_all_run_states(project_root: &Path) -> Vec<RunState> {
    let dir = project_root.join(".arccode").join("autonomous");
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in read {
        let Ok(e) = e else { continue };
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if let Ok(state) = load_state(&e.path()) {
            out.push(state);
        }
    }
    out
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
            spawned_at: Some("2026-05-27T14:30:00Z".into()),
            current_tool: Some("edit_file".into()),
            usd: 0.21,
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

    #[test]
    fn ascii_layout_puts_tasks_and_agents_in_one_row() {
        let view = render_dashboard(&sample_state(), &[]);
        let ascii = view.to_ascii();
        // The two top boxes share a single physical row → a line carries
        // both the Tasks and the Agents box header.
        let has_grid_row = ascii
            .lines()
            .any(|l| l.contains("─ Tasks") && l.contains("─ Agents"));
        assert!(has_grid_row, "Tasks and Agents should be side by side:\n{ascii}");
        // Live log spans below, on its own row.
        assert!(ascii.lines().any(|l| l.contains("─ Live log")));
    }

    #[test]
    fn task_row_surfaces_cost_deps_and_writes() {
        let mut state = sample_state();
        let t = state.task_mut("t2").unwrap();
        t.usd = 0.05;
        t.writes = vec!["a.rs".into(), "b.rs".into()];
        let model = build_model(&state, &[], None);
        let row = model.tasks.iter().find(|r| r.id == "t2").unwrap();
        assert_eq!(row.writes, 2);
        assert_eq!(row.deps, vec!["t1".to_string()]);
        let line = task_row_line(row);
        assert!(line.contains("$0.05"), "cost missing: {line}");
        assert!(line.contains("deps: t1"), "deps missing: {line}");
        assert!(line.contains("w2"), "write count missing: {line}");
    }

    #[test]
    fn header_reports_running_count_and_branch() {
        let model = build_model(&sample_state(), &[], None);
        assert_eq!(model.header.done, 1);
        assert_eq!(model.header.running, 1);
        assert_eq!(model.header.branch, "arccode/auto/2026-05-27-1430-abc");
        assert_eq!(model.header.base_short, "deadbeef");
        let bar = model.header.top_bar();
        assert!(bar.contains("1▶"), "running badge missing: {bar}");
    }

    #[test]
    fn task_elapsed_derives_from_start_and_end_stamps() {
        let mut state = sample_state();
        {
            let t = state.task_mut("t1").unwrap();
            t.started_at = Some("2026-05-27T14:30:00Z".into());
            t.ended_at = Some("2026-05-27T14:31:20Z".into());
        }
        let model = build_model(&state, &[], None);
        let row = model.tasks.iter().find(|r| r.id == "t1").unwrap();
        assert_eq!(row.elapsed_secs, Some(80));
        assert!(task_row_line(row).contains("1m20s"));
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
