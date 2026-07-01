//! Interactive ratatui UI for `arccode pilot watch`.
//!
//! Renders the same [`DashboardModel`] as `pilot status`, but as a live,
//! colour-coded, scrollable terminal UI laid out in a 2-column grid:
//!
//! ```text
//! ┌ Pilot: … ───────────────────────────────────┐
//! ┌ Tasks ───────────────┐┌ Agents ─────────────┐
//! │ …                    ││ …                   │
//! └──────────────────────┘└─────────────────────┘
//! ┌ Live log ───────────────────────────────────┐
//! │ …                                            │
//! └──────────────────────────────────────────────┘
//! ```
//!
//! The polling model is identical to the plain `watch` loop — it watches
//! `<run-dir>/state.json`'s mtime and only reloads when it advances — but
//! input is drained every ~120 ms so scrolling and quitting stay snappy.
//! Terminal raw-mode / alternate-screen setup is torn down on every exit
//! path (including errors) so the shell is always left clean.

use std::io::{self, Stdout};
use std::path::Path;
use std::process::ExitCode;
use std::time::SystemTime;

use anyhow::Result;
use chrono::Utc;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

use arccode_autonomous::dashboard::{self, AgentRow, DashboardModel, HeaderInfo, LogSeverity, TaskRow};
use arccode_autonomous::{AgentStatus, RunStatus, TaskStatus};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Live-watch `run_dir` in a full-screen ratatui UI. Blocks until the user
/// quits (`q` / `Esc` / `Ctrl-C`). `interval_ms` bounds the input-poll
/// cadence; state reloads are driven by the file mtime regardless.
pub fn run(run_dir: &Path, interval_ms: u64) -> Result<ExitCode> {
    let mut terminal = setup()?;
    // Whatever happens in the loop, always restore the terminal.
    let outcome = run_loop(&mut terminal, run_dir, interval_ms);
    teardown(&mut terminal)?;
    outcome
}

#[derive(Default)]
struct WatchUi {
    tasks_scroll: u16,
    last_mtime: Option<SystemTime>,
    model: Option<DashboardModel>,
    finished: bool,
}

impl WatchUi {
    /// Reload the run snapshot + a generous event tail for the log.
    fn reload(&mut self, run_dir: &Path) {
        self.last_mtime = dashboard::state_mtime(run_dir);
        if let (Ok(state), Ok(recent)) = (
            dashboard::load_state(run_dir),
            dashboard::tail_events(run_dir, 200),
        ) {
            self.finished = matches!(
                state.status,
                RunStatus::Done | RunStatus::Failed | RunStatus::Aborted
            );
            self.model = Some(dashboard::build_model(&state, &recent, Some(Utc::now())));
        }
    }
}

fn run_loop(terminal: &mut Term, run_dir: &Path, interval_ms: u64) -> Result<ExitCode> {
    let poll = std::time::Duration::from_millis(interval_ms.clamp(50, 250));
    let mut ui = WatchUi::default();
    ui.reload(run_dir);

    loop {
        terminal.draw(|f| draw(f, &mut ui))?;

        // Drain input first so keys feel responsive.
        if event::poll(poll)? {
            if let CtEvent::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Release {
                    continue;
                }
                match (k.code, k.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return Ok(ExitCode::SUCCESS),
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(ExitCode::SUCCESS),
                    (KeyCode::Up | KeyCode::Char('k'), _) => {
                        ui.tasks_scroll = ui.tasks_scroll.saturating_sub(1)
                    }
                    (KeyCode::Down | KeyCode::Char('j'), _) => {
                        ui.tasks_scroll = ui.tasks_scroll.saturating_add(1)
                    }
                    (KeyCode::PageUp, _) => ui.tasks_scroll = ui.tasks_scroll.saturating_sub(10),
                    (KeyCode::PageDown, _) => ui.tasks_scroll = ui.tasks_scroll.saturating_add(10),
                    (KeyCode::Home | KeyCode::Char('g'), _) => ui.tasks_scroll = 0,
                    _ => {}
                }
            }
        }

        // Reload only when state.json advances (cheap mtime probe).
        if dashboard::state_mtime(run_dir) != ui.last_mtime {
            ui.reload(run_dir);
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn draw(f: &mut Frame, ui: &mut WatchUi) {
    let area = f.area();
    let Some(model) = ui.model.clone() else {
        let p = Paragraph::new("loading run…").block(bordered("Pilot"));
        f.render_widget(p, area);
        return;
    };

    // Rows: header (4) · grid (rest) · footer (1).
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    render_header(f, rows[0], &model.header);

    // Grid: top row (Tasks | Agents) then Live log full width.
    let grid = Layout::vertical([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(rows[1]);
    let top = Layout::horizontal([Constraint::Percentage(56), Constraint::Percentage(44)])
        .split(grid[0]);

    render_tasks(f, top[0], &model.tasks, &mut ui.tasks_scroll);
    render_agents(f, top[1], &model.agents);
    render_log(f, grid[1], &model.log);
    render_footer(f, rows[2], ui.finished);
}

fn render_header(f: &mut Frame, area: Rect, h: &HeaderInfo) {
    let (status_label, status_color) = run_status_style(h.status);
    let mut counts = vec![
        Span::styled(format!("{}", h.done), Style::default().fg(Color::Green)),
        Span::raw(format!("/{} done", h.total)),
    ];
    if h.running > 0 {
        counts.push(Span::raw(" · "));
        counts.push(Span::styled(
            format!("{}▶ running", h.running),
            Style::default().fg(Color::Cyan),
        ));
    }
    if h.failed > 0 {
        counts.push(Span::raw(" · "));
        counts.push(Span::styled(
            format!("{}✗ failed", h.failed),
            Style::default().fg(Color::Red),
        ));
    }
    if h.blocked > 0 {
        counts.push(Span::raw(" · "));
        counts.push(Span::styled(
            format!("{}‼ blocked", h.blocked),
            Style::default().fg(Color::Yellow),
        ));
    }

    let line1 = Line::from(
        [
            vec![
                Span::styled(
                    h.run_id.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(status_label, Style::default().fg(status_color)),
                Span::raw("   "),
            ],
            counts,
        ]
        .concat(),
    );

    let mut meta = vec![
        Span::styled("elapsed ", dim()),
        Span::raw(h.elapsed_secs.map(fmt_dur).unwrap_or_else(|| "—".into())),
        Span::styled("  ·  spend ", dim()),
        Span::styled(format!("${:.2}", h.usd), Style::default().fg(Color::Green)),
        Span::styled("  ·  branch ", dim()),
        Span::raw(h.branch.clone()),
    ];
    if !h.base_short.is_empty() {
        meta.push(Span::styled("  ·  base ", dim()));
        meta.push(Span::raw(h.base_short.clone()));
    }

    let p = Paragraph::new(vec![line1, Line::from(meta)]).block(bordered("Pilot"));
    f.render_widget(p, area);
}

fn render_tasks(f: &mut Frame, area: Rect, tasks: &[TaskRow], scroll: &mut u16) {
    let lines: Vec<Line> = tasks.iter().map(task_line).collect();
    // Clamp scroll so we never page past the end.
    let inner_h = area.height.saturating_sub(2);
    let max = (lines.len() as u16).saturating_sub(inner_h);
    if *scroll > max {
        *scroll = max;
    }
    let title = format!("Tasks ({})", tasks.len());
    let p = Paragraph::new(lines)
        .block(bordered(&title))
        .scroll((*scroll, 0));
    f.render_widget(p, area);
}

fn render_agents(f: &mut Frame, area: Rect, agents: &[AgentRow]) {
    let lines: Vec<Line> = if agents.is_empty() {
        vec![Line::from(Span::styled("(no agents yet)", dim()))]
    } else {
        agents.iter().map(agent_line).collect()
    };
    let title = format!("Agents ({})", agents.len());
    f.render_widget(Paragraph::new(lines).block(bordered(&title)), area);
}

fn render_log(f: &mut Frame, area: Rect, log: &[LogSeverityLine]) {
    let lines: Vec<Line> = log.iter().map(log_line).collect();
    // Stick to the bottom so the newest events are always visible.
    let inner_h = area.height.saturating_sub(2);
    let scroll = (lines.len() as u16).saturating_sub(inner_h);
    f.render_widget(
        Paragraph::new(lines)
            .block(bordered("Live log"))
            .scroll((scroll, 0)),
        area,
    );
}

fn render_footer(f: &mut Frame, area: Rect, finished: bool) {
    let hint = if finished {
        Line::from(Span::styled(
            " run finished — press q to exit ",
            Style::default().fg(Color::Green),
        ))
    } else {
        Line::from(vec![
            Span::styled(" ↑/↓", Style::default().fg(Color::Cyan)),
            Span::styled(" scroll tasks · ", dim()),
            Span::styled("q", Style::default().fg(Color::Cyan)),
            Span::styled(" quit ", dim()),
        ])
    };
    f.render_widget(Paragraph::new(hint), area);
}

// ---------------------------------------------------------------------------
// Row → styled Line
// ---------------------------------------------------------------------------

fn task_line(t: &TaskRow) -> Line<'static> {
    let (glyph, color) = task_status_style(t.status);
    let mut spans = vec![
        Span::styled(format!(" {glyph} "), Style::default().fg(color)),
        Span::styled(
            format!("{:<4}", t.id),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("[{}] ", t.role), dim()),
        Span::raw(t.title.clone()),
    ];

    let mut meta: Vec<String> = Vec::new();
    if let Some(a) = &t.agent {
        meta.push(a.clone());
    }
    if !t.deps.is_empty() {
        meta.push(format!("deps: {}", t.deps.join(",")));
    }
    if t.writes > 0 {
        meta.push(format!("✎{}", t.writes));
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
    if !meta.is_empty() {
        spans.push(Span::styled(format!("  · {}", meta.join(" · ")), dim()));
    }
    Line::from(spans)
}

fn agent_line(a: &AgentRow) -> Line<'static> {
    let (glyph, color) = agent_status_style(a.status);
    let mut spans = vec![
        Span::styled(format!(" {glyph} "), Style::default().fg(color)),
        Span::styled(
            format!("{} ", a.id),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("[{}] ", a.role), dim()),
    ];
    let task = a
        .task
        .as_deref()
        .map(|t| format!("task={t}"))
        .unwrap_or_else(|| "idle".into());
    spans.push(Span::raw(task));

    let mut meta: Vec<String> = Vec::new();
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
    if !meta.is_empty() {
        spans.push(Span::styled(format!("  · {}", meta.join(" · ")), dim()));
    }
    Line::from(spans)
}

/// The dashboard's `LogRow` (aliased here for readability).
type LogSeverityLine = arccode_autonomous::dashboard::LogRow;

fn log_line(r: &LogSeverityLine) -> Line<'static> {
    let color = match r.severity {
        LogSeverity::Ok => Color::Green,
        LogSeverity::Warn => Color::Yellow,
        LogSeverity::Error => Color::Red,
        LogSeverity::Info => Color::Gray,
    };
    Line::from(Span::styled(r.text.clone(), Style::default().fg(color)))
}

// ---------------------------------------------------------------------------
// Style helpers
// ---------------------------------------------------------------------------

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn bordered(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
}

fn task_status_style(s: TaskStatus) -> (char, Color) {
    match s {
        TaskStatus::Pending => ('·', Color::DarkGray),
        TaskStatus::Todo => ('○', Color::Blue),
        TaskStatus::InProgress => ('↻', Color::Cyan),
        TaskStatus::Review => ('◇', Color::Magenta),
        TaskStatus::Done => ('✓', Color::Green),
        TaskStatus::Failed => ('✗', Color::Red),
        TaskStatus::Blocked => ('‼', Color::Yellow),
    }
}

fn agent_status_style(s: AgentStatus) -> (char, Color) {
    match s {
        AgentStatus::Idle => ('·', Color::DarkGray),
        AgentStatus::InProgress => ('↻', Color::Cyan),
        AgentStatus::Done => ('✓', Color::Green),
        AgentStatus::Failed => ('✗', Color::Red),
        AgentStatus::Aborted => ('⊘', Color::Yellow),
    }
}

fn run_status_style(s: RunStatus) -> (String, Color) {
    let (label, color) = match s {
        RunStatus::Planning => ("planning", Color::Blue),
        RunStatus::AwaitingApproval => ("awaiting-approval", Color::Yellow),
        RunStatus::Running => ("running", Color::Cyan),
        RunStatus::Merging => ("merging", Color::Magenta),
        RunStatus::Done => ("done", Color::Green),
        RunStatus::Failed => ("failed", Color::Red),
        RunStatus::Aborted => ("aborted", Color::Yellow),
    };
    (label.to_string(), color)
}

/// Compact duration: `45s`, `1m20s`, `2h03m`.
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

// ---------------------------------------------------------------------------
// Terminal setup / teardown
// ---------------------------------------------------------------------------

fn setup() -> Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

fn teardown(terminal: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
