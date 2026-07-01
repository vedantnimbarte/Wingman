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
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyCode, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Sparkline, Wrap};
use ratatui::{Frame, Terminal};

use arccode_autonomous::control::{self, ControlCommand};
use arccode_autonomous::dashboard::{
    self, AgentRow, DashboardModel, HeaderInfo, LogRow, LogSeverity, RunSummary, TaskRow,
};
use arccode_autonomous::{AgentStatus, RunStatus, TaskStatus};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Live-watch the pilot runs under `project_root` in a full-screen ratatui
/// UI, starting on `initial` (or the newest run). When more than one run is
/// active, a Runs sidebar appears and you can switch between them. `ascii`
/// forces the plain-ASCII glyph set for terminals that can't render the
/// unicode ones. Blocks until the user quits (`q` / `Esc` / `Ctrl-C`).
pub fn run(
    project_root: &Path,
    initial: Option<String>,
    interval_ms: u64,
    ascii: bool,
) -> Result<ExitCode> {
    let mut terminal = setup()?;
    // Whatever happens in the loop, always restore the terminal.
    let outcome = run_loop(
        &mut terminal,
        project_root,
        initial,
        interval_ms,
        Glyphs { ascii },
    );
    teardown(&mut terminal)?;
    outcome
}

/// Which pane the arrow keys drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Focus {
    Runs,
    #[default]
    Tasks,
    Log,
}

/// Minimum severity the Live log shows. Cycled with `f`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SevFilter {
    #[default]
    All,
    /// Warnings and errors only.
    Warn,
    /// Errors only.
    Error,
}

impl SevFilter {
    fn next(self) -> Self {
        match self {
            SevFilter::All => SevFilter::Warn,
            SevFilter::Warn => SevFilter::Error,
            SevFilter::Error => SevFilter::All,
        }
    }

    fn accepts(self, s: LogSeverity) -> bool {
        match self {
            SevFilter::All => true,
            SevFilter::Warn => matches!(s, LogSeverity::Warn | LogSeverity::Error),
            SevFilter::Error => matches!(s, LogSeverity::Error),
        }
    }

    fn label(self) -> Option<&'static str> {
        match self {
            SevFilter::All => None,
            SevFilter::Warn => Some("warn+"),
            SevFilter::Error => Some("errors"),
        }
    }
}

/// Scroll / filter state for the Live log pane. By default the log follows
/// the newest events; scrolling up detaches it, and `End`/`G` re-attaches.
#[derive(Debug, Clone, Default)]
struct LogView {
    /// Top row offset when not following. Kept clamped by the renderer.
    scroll: u16,
    /// Stick to the bottom (newest) as fresh events arrive.
    follow: bool,
    severity: SevFilter,
    /// Case-insensitive substring filter (matches agent names, task ids, …).
    query: String,
    /// True while the user is typing into the `/` search box.
    editing: bool,
}

impl LogView {
    fn new() -> Self {
        Self {
            follow: true,
            ..Default::default()
        }
    }

    /// Whether a log row passes the active severity + text filters.
    fn accepts(&self, r: &LogRow) -> bool {
        self.severity.accepts(r.severity)
            && (self.query.is_empty() || r.text.to_lowercase().contains(&self.query.to_lowercase()))
    }

    /// Pane title reflecting the follow state and any active filters.
    fn title(&self, shown: usize, total: usize) -> String {
        let mut t = String::from("Live log");
        if let Some(sev) = self.severity.label() {
            t.push_str(&format!(" [{sev}]"));
        }
        if self.editing {
            t.push_str(&format!(" /{}_", self.query));
        } else if !self.query.is_empty() {
            t.push_str(&format!(" /{}", self.query));
        }
        if shown != total {
            t.push_str(&format!("  {shown}/{total}"));
        }
        if !self.follow {
            t.push_str("  (paused)");
        }
        t
    }
}

/// The glyph set the UI draws with. Unicode by default; the ASCII variant is
/// a portable fallback for terminals (legacy Windows console, non-UTF-8
/// locales) that render the fancier glyphs as tofu boxes.
#[derive(Debug, Clone, Copy)]
struct Glyphs {
    ascii: bool,
}

impl Glyphs {
    /// One glyph or the other, chosen by mode. Keeps the call sites readable.
    fn pick(&self, unicode: char, ascii: char) -> char {
        if self.ascii {
            ascii
        } else {
            unicode
        }
    }

    /// Animated progress spinner frame. Unicode rotates a quarter-filled
    /// disc; ASCII falls back to the classic `|/-\` spinner.
    fn spinner(&self, frame: u64) -> char {
        const UNI: [char; 4] = ['◐', '◓', '◑', '◒'];
        const ASC: [char; 4] = ['|', '/', '-', '\\'];
        let set = if self.ascii { &ASC } else { &UNI };
        set[(frame as usize) % set.len()]
    }

    fn current(&self) -> char {
        self.pick('▸', '>')
    }
    fn tool(&self) -> char {
        self.pick('▸', '>')
    }
    fn writes(&self) -> char {
        self.pick('✎', 'w')
    }
    fn running(&self) -> char {
        self.pick('▶', '>')
    }
    fn failed(&self) -> char {
        self.pick('✗', 'x')
    }
    fn blocked(&self) -> char {
        self.pick('‼', '!')
    }
}

struct WatchUi {
    /// Runs offered in the sidebar — active (non-terminal) runs, plus the
    /// currently-watched run even if it has since finished.
    runs: Vec<RunSummary>,
    /// Index into `runs` of the run being watched.
    current: usize,
    focus: Focus,
    /// Highlighted row in the Tasks pane; `Enter` opens its detail overlay.
    tasks_sel: usize,
    /// Task count from the last reload, so selection nav can clamp.
    tasks_len: usize,
    log: LogView,
    /// When `Some`, a task-detail overlay is open for this task id.
    detail: Option<String>,
    /// Whether the `?` keybinding help overlay is open.
    help: bool,
    /// A pending destructive action awaiting y/n confirmation.
    confirm: Option<Confirm>,
    /// One-shot status line shown after a control action is dispatched.
    toast: Option<String>,
    last_mtime: Option<SystemTime>,
    model: Option<DashboardModel>,
    finished: bool,
    /// Failed-task count last seen, so a *new* failure rings the bell once.
    seen_failed: usize,
    /// Whether the finish bell has already fired for the watched run.
    bell_finish_sent: bool,
    /// Set when something noteworthy happened this reload; the loop rings the
    /// terminal bell and clears it.
    ring_bell: bool,
    /// Animation frame, advanced off wall-clock time so the in-progress
    /// spinner rotates smoothly regardless of the state-poll cadence.
    frame: u64,
    /// Cumulative spend samples (USD cents) captured on each reload, for the
    /// header sparkline. Bounded so it never grows without limit.
    spend_samples: Vec<u64>,
    /// Last-drawn pane rectangles + task scroll offset, so mouse clicks and
    /// wheel events can be hit-tested against what's on screen.
    hit: HitAreas,
    /// Glyph set the UI renders with (unicode or ASCII fallback).
    glyphs: Glyphs,
}

/// A destructive control action queued behind a y/n confirmation prompt.
#[derive(Debug, Clone)]
struct Confirm {
    prompt: String,
    cmd: ControlCommand,
}

/// Screen geometry captured each frame for mouse hit-testing.
#[derive(Debug, Clone, Copy, Default)]
struct HitAreas {
    runs: Option<Rect>,
    tasks: Option<Rect>,
    log: Option<Rect>,
    /// The task pane's scroll offset at draw time, so a click maps to the
    /// right row.
    tasks_scroll: u16,
}

impl HitAreas {
    /// Which pane, if any, contains the point — for wheel + click routing.
    fn pane_at(&self, x: u16, y: u16) -> Option<Focus> {
        let inside = |r: &Option<Rect>| r.map(|r| contains(r, x, y)).unwrap_or(false);
        if inside(&self.runs) {
            Some(Focus::Runs)
        } else if inside(&self.tasks) {
            Some(Focus::Tasks)
        } else if inside(&self.log) {
            Some(Focus::Log)
        } else {
            None
        }
    }
}

/// Whether `(x, y)` falls inside a rectangle.
fn contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

/// The 0-based content row a click landed on inside a bordered pane, or
/// `None` when it hit the border. Assumes a 1-cell border.
fn row_in(r: Rect, y: u16) -> Option<usize> {
    if y <= r.y || y + 1 >= r.y + r.height {
        return None;
    }
    Some((y - r.y - 1) as usize)
}

/// Cap on the spend sparkline history — a couple of terminal-widths of points.
const SPEND_SAMPLES_MAX: usize = 240;

impl WatchUi {
    fn new(runs: Vec<RunSummary>, current: usize, glyphs: Glyphs) -> Self {
        Self {
            runs,
            current,
            focus: Focus::default(),
            tasks_sel: 0,
            tasks_len: 0,
            log: LogView::new(),
            detail: None,
            help: false,
            confirm: None,
            toast: None,
            last_mtime: None,
            model: None,
            finished: false,
            seen_failed: 0,
            bell_finish_sent: false,
            ring_bell: false,
            frame: 0,
            spend_samples: Vec::new(),
            hit: HitAreas::default(),
            glyphs,
        }
    }

    /// True once the sidebar is worth showing.
    fn show_runs(&self) -> bool {
        self.runs.len() > 1
    }

    fn current_dir(&self) -> Option<&Path> {
        self.runs.get(self.current).map(|r| r.dir.as_path())
    }

    /// Reload the current run's snapshot + a generous event tail for the log.
    fn reload(&mut self) {
        let Some(dir) = self.current_dir().map(Path::to_path_buf) else {
            return;
        };
        self.last_mtime = dashboard::state_mtime(&dir);
        if let (Ok(state), Ok(recent)) = (
            dashboard::load_state(&dir),
            dashboard::tail_events(&dir, 200),
        ) {
            self.finished = matches!(
                state.status,
                RunStatus::Done | RunStatus::Failed | RunStatus::Aborted
            );
            let model = dashboard::build_model(&state, &recent, Some(Utc::now()));
            self.tasks_len = model.tasks.len();
            // Keep the selection in-bounds as tasks come and go.
            self.tasks_sel = self.tasks_sel.min(self.tasks_len.saturating_sub(1));
            self.push_spend_sample(model.header.usd);

            // Ring the bell once on a new failure and once when the run ends,
            // so an unattended watch surfaces trouble without staring at it.
            self.note_bell_events(model.header.failed, self.finished);

            self.model = Some(model);
        }
    }

    /// Set `ring_bell` when the failed-task count grows or the run first
    /// reaches a terminal state. Pure so it's unit-testable without file IO.
    fn note_bell_events(&mut self, failed: usize, finished: bool) {
        if failed > self.seen_failed {
            self.ring_bell = true;
        }
        self.seen_failed = failed;
        if finished && !self.bell_finish_sent {
            self.ring_bell = true;
            self.bell_finish_sent = true;
        }
    }

    /// Consume the pending-bell flag (true at most once per noteworthy event).
    fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.ring_bell)
    }

    /// Record the current cumulative spend for the header sparkline, keeping
    /// the history bounded.
    fn push_spend_sample(&mut self, usd: f64) {
        self.spend_samples
            .push((usd * 100.0).round().max(0.0) as u64);
        let overflow = self.spend_samples.len().saturating_sub(SPEND_SAMPLES_MAX);
        if overflow > 0 {
            self.spend_samples.drain(..overflow);
        }
    }

    /// Switch the watched run to `idx` and reload it.
    fn switch_to(&mut self, idx: usize) {
        if idx < self.runs.len() && idx != self.current {
            self.current = idx;
            self.tasks_sel = 0;
            self.detail = None;
            self.confirm = None;
            self.toast = None;
            // The sparkline and bell state are per-run; start fresh.
            self.spend_samples.clear();
            self.seen_failed = 0;
            self.bell_finish_sent = false;
            self.reload();
        }
    }

    fn select_prev(&mut self) {
        if self.current > 0 {
            self.switch_to(self.current - 1);
        }
    }

    fn select_next(&mut self) {
        if self.current + 1 < self.runs.len() {
            self.switch_to(self.current + 1);
        }
    }

    /// Advance focus through the visible panes: Tasks → Log → (Runs) → Tasks.
    /// The Runs pane is only in the cycle when the sidebar is shown.
    fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Tasks => Focus::Log,
            Focus::Log if self.show_runs() => Focus::Runs,
            Focus::Log => Focus::Tasks,
            Focus::Runs => Focus::Tasks,
        };
    }

    /// Largest valid task-selection index (0 when there are no tasks).
    fn tasks_max(&self) -> usize {
        self.tasks_len.saturating_sub(1)
    }

    /// `↑` / `k` in the focused pane.
    fn nav_up(&mut self) {
        match self.focus {
            Focus::Runs => self.select_prev(),
            Focus::Tasks => self.tasks_sel = self.tasks_sel.saturating_sub(1),
            Focus::Log => {
                self.log.follow = false;
                self.log.scroll = self.log.scroll.saturating_sub(1);
            }
        }
    }

    /// `↓` / `j` in the focused pane.
    fn nav_down(&mut self) {
        match self.focus {
            Focus::Runs => self.select_next(),
            Focus::Tasks => self.tasks_sel = (self.tasks_sel + 1).min(self.tasks_max()),
            Focus::Log => {
                self.log.follow = false;
                self.log.scroll = self.log.scroll.saturating_add(1);
            }
        }
    }

    /// Page-sized jump (`PgUp`/`PgDn`) in the focused scroll pane.
    fn nav_page(&mut self, down: bool) {
        const PAGE: usize = 10;
        match self.focus {
            Focus::Runs => {}
            Focus::Tasks => {
                self.tasks_sel = if down {
                    (self.tasks_sel + PAGE).min(self.tasks_max())
                } else {
                    self.tasks_sel.saturating_sub(PAGE)
                };
            }
            Focus::Log => {
                self.log.follow = false;
                self.log.scroll = if down {
                    self.log.scroll.saturating_add(PAGE as u16)
                } else {
                    self.log.scroll.saturating_sub(PAGE as u16)
                };
            }
        }
    }

    /// `Home` / `g`: jump to the top of the focused pane.
    fn nav_home(&mut self) {
        match self.focus {
            Focus::Runs => self.switch_to(0),
            Focus::Tasks => self.tasks_sel = 0,
            Focus::Log => {
                self.log.follow = false;
                self.log.scroll = 0;
            }
        }
    }

    /// `End` / `G`: jump to the bottom of the focused pane. For the log this
    /// re-attaches follow-mode.
    fn nav_end(&mut self) {
        match self.focus {
            Focus::Runs => self.switch_to(self.runs.len().saturating_sub(1)),
            Focus::Tasks => self.tasks_sel = self.tasks_max(),
            Focus::Log => self.log.follow = true,
        }
    }

    /// The currently highlighted task id, if any.
    fn selected_task_id(&self) -> Option<String> {
        self.model
            .as_ref()
            .and_then(|m| m.tasks.get(self.tasks_sel))
            .map(|t| t.id.clone())
    }

    /// Whether the watched run is parked at the plan-approval gate.
    fn awaiting_approval(&self) -> bool {
        self.model
            .as_ref()
            .map(|m| m.header.status == RunStatus::AwaitingApproval)
            .unwrap_or(false)
    }

    /// Append a control command to the current run's control channel and show
    /// a short confirmation toast.
    fn send_control(&mut self, cmd: ControlCommand, note: &str) {
        let Some(dir) = self.current_dir().map(Path::to_path_buf) else {
            return;
        };
        match control::append(&dir, &cmd) {
            Ok(()) => self.toast = Some(note.to_string()),
            Err(e) => self.toast = Some(format!("control write failed: {e}")),
        }
    }

    /// Mouse-wheel over a pane scrolls it (falling back to the focused pane
    /// when the pointer is off any pane).
    fn on_scroll(&mut self, down: bool, x: u16, y: u16) {
        let pane = self.hit.pane_at(x, y).unwrap_or(self.focus);
        match pane {
            Focus::Runs => {
                if down {
                    self.select_next()
                } else {
                    self.select_prev()
                }
            }
            Focus::Tasks => {
                self.tasks_sel = if down {
                    (self.tasks_sel + 1).min(self.tasks_max())
                } else {
                    self.tasks_sel.saturating_sub(1)
                };
            }
            Focus::Log => {
                self.log.follow = false;
                self.log.scroll = if down {
                    self.log.scroll.saturating_add(3)
                } else {
                    self.log.scroll.saturating_sub(3)
                };
            }
        }
    }

    /// Left-click focuses the clicked pane and, in the Runs/Tasks lists,
    /// selects the clicked row.
    fn on_click(&mut self, x: u16, y: u16) {
        match self.hit.pane_at(x, y) {
            Some(Focus::Runs) => {
                self.focus = Focus::Runs;
                if let Some(rect) = self.hit.runs {
                    if let Some(row) = row_in(rect, y) {
                        if row < self.runs.len() {
                            self.switch_to(row);
                        }
                    }
                }
            }
            Some(Focus::Tasks) => {
                self.focus = Focus::Tasks;
                if let Some(rect) = self.hit.tasks {
                    if let Some(row) = row_in(rect, y) {
                        let idx = self.hit.tasks_scroll as usize + row;
                        if idx < self.tasks_len {
                            self.tasks_sel = idx;
                        }
                    }
                }
            }
            Some(Focus::Log) => self.focus = Focus::Log,
            None => {}
        }
    }

    /// Re-list runs (active + the watched one), preserving the current
    /// selection by id and refreshing each run's progress/status.
    fn refresh_runs(&mut self, project_root: &Path) {
        let all = dashboard::list_runs(project_root).unwrap_or_default();
        let current_id = self.runs.get(self.current).map(|r| r.run_id.clone());
        let list = active_plus(all, current_id.as_deref());

        self.current = current_id
            .and_then(|cid| list.iter().position(|r| r.run_id == cid))
            .unwrap_or(0)
            .min(list.len().saturating_sub(1));
        self.runs = list;
        // Focus can't sit on a hidden sidebar.
        if !self.show_runs() && self.focus == Focus::Runs {
            self.focus = Focus::Tasks;
        }
    }
}

/// Active (non-terminal) runs, plus `keep` (the watched run) even if it has
/// finished — so a run doesn't vanish from the sidebar mid-watch, and
/// `watch <finished-id>` still shows something. Falls back to *all* runs when
/// nothing is active, so the UI is never blank.
fn active_plus(all: Vec<RunSummary>, keep: Option<&str>) -> Vec<RunSummary> {
    let mut list: Vec<RunSummary> = all.iter().filter(|r| !r.is_terminal()).cloned().collect();
    if let Some(id) = keep {
        if !list.iter().any(|r| r.run_id == id) {
            if let Some(r) = all.iter().find(|r| r.run_id == id) {
                list.push(r.clone());
            }
        }
    }
    if list.is_empty() {
        list = all;
    }
    list
}

/// Initial run list + the index to start on.
fn initial_runs(project_root: &Path, initial: &Option<String>) -> (Vec<RunSummary>, usize) {
    let all = dashboard::list_runs(project_root).unwrap_or_default();
    let list = active_plus(all, initial.as_deref());
    let current = initial
        .as_ref()
        .and_then(|id| list.iter().position(|r| &r.run_id == id))
        .unwrap_or(0);
    (list, current)
}

fn run_loop(
    terminal: &mut Term,
    project_root: &Path,
    initial: Option<String>,
    interval_ms: u64,
    glyphs: Glyphs,
) -> Result<ExitCode> {
    // Cap the wait so we repaint at least every ~120 ms — enough to animate
    // the spinner smoothly even when the user picked a slow --interval-ms.
    let poll = std::time::Duration::from_millis(interval_ms.clamp(50, 120));
    let started = std::time::Instant::now();

    let (runs, current) = initial_runs(project_root, &initial);
    if runs.is_empty() {
        return Ok(ExitCode::from(1));
    }
    let mut ui = WatchUi::new(runs, current, glyphs);
    ui.reload();

    // Throttle the (relatively expensive) full run-list rescan.
    let mut last_list = std::time::Instant::now();
    let list_every = std::time::Duration::from_millis(1000);

    loop {
        // ~8 frames/sec: one spinner step per redraw at the 120 ms cap.
        ui.frame = (started.elapsed().as_millis() / 120) as u64;
        terminal.draw(|f| draw(f, &mut ui))?;

        // Audible cue for a new failure or run completion, unless muted.
        if ui.take_bell() {
            emit_bell();
        }

        // Drain input first so keys feel responsive.
        if event::poll(poll)? {
            match event::read()? {
                CtEvent::Key(k) => {
                    if k.kind == KeyEventKind::Release {
                        continue;
                    }
                    // Ctrl-C always quits, even mid-search.
                    if let (KeyCode::Char('c'), KeyModifiers::CONTROL) = (k.code, k.modifiers) {
                        return Ok(ExitCode::SUCCESS);
                    }
                    // The help overlay is modal: any key closes it.
                    if ui.help {
                        ui.help = false;
                        continue;
                    }
                    // A confirmation prompt is modal: y/Enter runs the queued
                    // action, anything else cancels.
                    if ui.confirm.is_some() {
                        if matches!(k.code, KeyCode::Char('y') | KeyCode::Enter) {
                            let c = ui.confirm.take().unwrap();
                            let note = describe_cmd(&c.cmd);
                            ui.send_control(c.cmd, &note);
                        } else {
                            ui.confirm = None;
                        }
                        continue;
                    }
                    // A detail overlay is modal: Esc/Enter/q dismiss it, everything
                    // else is swallowed so it doesn't drive the panes underneath.
                    if ui.detail.is_some() {
                        if matches!(k.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                            ui.detail = None;
                        }
                        continue;
                    }
                    // While typing a `/` search, keys edit the query instead of
                    // driving the panes.
                    if ui.log.editing {
                        match k.code {
                            KeyCode::Esc => {
                                ui.log.editing = false;
                                ui.log.query.clear();
                            }
                            KeyCode::Enter => ui.log.editing = false,
                            KeyCode::Backspace => {
                                ui.log.query.pop();
                            }
                            KeyCode::Char(c) => ui.log.query.push(c),
                            _ => {}
                        }
                        continue;
                    }
                    match (k.code, k.modifiers) {
                        (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => {
                            return Ok(ExitCode::SUCCESS)
                        }
                        (KeyCode::Tab, _) => ui.cycle_focus(),
                        (KeyCode::Up | KeyCode::Char('k'), _) => ui.nav_up(),
                        (KeyCode::Down | KeyCode::Char('j'), _) => ui.nav_down(),
                        (KeyCode::PageUp, _) => ui.nav_page(false),
                        (KeyCode::PageDown, _) => ui.nav_page(true),
                        (KeyCode::Home | KeyCode::Char('g'), _) => ui.nav_home(),
                        (KeyCode::End | KeyCode::Char('G'), _) => ui.nav_end(),
                        // Enter opens the detail overlay for the highlighted task.
                        (KeyCode::Enter, _) => ui.detail = ui.selected_task_id(),
                        // Log filtering: `/` opens the search box, `f` cycles the
                        // severity filter. Both focus the log so the effect is
                        // visible immediately.
                        (KeyCode::Char('/'), _) => {
                            ui.focus = Focus::Log;
                            ui.log.editing = true;
                        }
                        (KeyCode::Char('f'), _) => {
                            ui.focus = Focus::Log;
                            ui.log.severity = ui.log.severity.next();
                        }
                        (KeyCode::Char('?'), _) => ui.help = true,
                        // Run control (writes to the run's control channel).
                        // Abort is destructive → confirm first.
                        (KeyCode::Char('x'), _) => {
                            ui.confirm = Some(Confirm {
                                prompt: "Abort this run? (y/n)".into(),
                                cmd: ControlCommand::AbortRun,
                            });
                        }
                        (KeyCode::Char('r'), _) => {
                            if let Some(id) = ui.selected_task_id() {
                                ui.confirm = Some(Confirm {
                                    prompt: format!("Retry task {id}? (y/n)"),
                                    cmd: ControlCommand::RetryTask { id },
                                });
                            }
                        }
                        // Approve / veto are the expected response to a gate,
                        // so they act directly — but only while awaiting.
                        (KeyCode::Char('a'), _) if ui.awaiting_approval() => {
                            ui.send_control(ControlCommand::Approve, "approved");
                        }
                        (KeyCode::Char('v'), _) if ui.awaiting_approval() => {
                            ui.send_control(ControlCommand::Veto, "vetoed");
                        }
                        // Number keys jump straight to a run, regardless of focus.
                        (KeyCode::Char(c), _) if c.is_ascii_digit() && c != '0' => {
                            ui.switch_to(c as usize - '1' as usize);
                        }
                        _ => {}
                    }
                }
                // Mouse: wheel scrolls the pane under the pointer, left-click
                // focuses/selects. Ignored while an overlay is modal.
                CtEvent::Mouse(m) if ui.detail.is_none() && !ui.help && ui.confirm.is_none() => {
                    match m.kind {
                        MouseEventKind::ScrollDown => ui.on_scroll(true, m.column, m.row),
                        MouseEventKind::ScrollUp => ui.on_scroll(false, m.column, m.row),
                        MouseEventKind::Down(MouseButton::Left) => ui.on_click(m.column, m.row),
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        // Refresh the run list periodically (new runs, status changes).
        if last_list.elapsed() >= list_every {
            ui.refresh_runs(project_root);
            last_list = std::time::Instant::now();
        }

        // Reload the current run's state when its state.json advances.
        if let Some(dir) = ui.current_dir().map(Path::to_path_buf) {
            if dashboard::state_mtime(&dir) != ui.last_mtime {
                ui.reload();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn draw(f: &mut Frame, ui: &mut WatchUi) {
    let area = f.area();
    let g = ui.glyphs;
    let Some(model) = ui.model.clone() else {
        let p = Paragraph::new("loading run…").block(bordered("Pilot"));
        f.render_widget(p, area);
        return;
    };

    // Rows: header (4) · meters (3) · grid (rest) · footer (1).
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    render_header(f, rows[0], &model.header, g);
    render_meters(f, rows[1], &model.header, &ui.spend_samples);

    // Grid: top row then Live log full width. When >1 run is active, the
    // top row gains a Runs sidebar on the left: Runs | Tasks | Agents.
    let grid =
        Layout::vertical([Constraint::Percentage(62), Constraint::Percentage(38)]).split(rows[2]);

    let (work, runs_rect) = if ui.show_runs() {
        let cols = Layout::horizontal([Constraint::Length(22), Constraint::Min(0)]).split(grid[0]);
        render_runs(f, cols[0], &ui.runs, ui.current, ui.focus, ui.frame, g);
        (cols[1], Some(cols[0]))
    } else {
        (grid[0], None)
    };
    let top =
        Layout::horizontal([Constraint::Percentage(56), Constraint::Percentage(44)]).split(work);

    let tasks_focused = ui.focus == Focus::Tasks;
    let tasks_scroll = render_tasks(
        f,
        top[0],
        &model.tasks,
        ui.tasks_sel,
        ui.frame,
        tasks_focused,
        g,
    );
    render_agents(f, top[1], &model.agents, ui.frame, g);
    let log_focused = ui.focus == Focus::Log;
    render_log(f, grid[1], &model.log, &mut ui.log, log_focused);
    render_footer(
        f,
        rows[3],
        ui.finished,
        ui.show_runs(),
        ui.awaiting_approval(),
        ui.toast.as_deref(),
    );

    // Capture geometry for mouse hit-testing on the next input tick.
    ui.hit = HitAreas {
        runs: runs_rect,
        tasks: Some(top[0]),
        log: Some(grid[1]),
        tasks_scroll,
    };

    // Overlays float above the grid when open. Confirm is the most modal,
    // then help, then the detail overlay.
    if let Some(c) = &ui.confirm {
        render_confirm(f, area, c);
    } else if ui.help {
        render_help(f, area, ui.glyphs);
    } else if let Some(id) = &ui.detail {
        render_detail(f, area, &model, id, g);
    }
}

/// Short past-tense description of a dispatched control command, for the toast.
fn describe_cmd(cmd: &ControlCommand) -> String {
    match cmd {
        ControlCommand::AbortRun => "run abort sent".into(),
        ControlCommand::AbortTask { id } => format!("abort sent for {id}"),
        ControlCommand::RetryTask { id } => format!("retry sent for {id}"),
        ControlCommand::Approve => "approved".into(),
        ControlCommand::Veto => "vetoed".into(),
    }
}

/// Small centred y/n confirmation prompt for a destructive action.
fn render_confirm(f: &mut Frame, area: Rect, c: &Confirm) {
    let rect = centered_rect(44, 20, area);
    f.render_widget(Clear, rect);
    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            c.prompt.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("y", Style::default().fg(Color::Green)),
            Span::styled(" confirm    ", dim()),
            Span::styled("n/Esc", Style::default().fg(Color::Red)),
            Span::styled(" cancel", dim()),
        ]),
    ];
    let p = Paragraph::new(lines)
        .block(bordered_focused("Confirm", true))
        .wrap(Wrap { trim: false });
    f.render_widget(p, rect);
}

/// The Runs sidebar: one row per active run, the watched one marked and the
/// focused selection highlighted. Running runs animate the spinner.
fn render_runs(
    f: &mut Frame,
    area: Rect,
    runs: &[RunSummary],
    current: usize,
    focus: Focus,
    frame: u64,
    g: Glyphs,
) {
    let lines: Vec<Line> = runs
        .iter()
        .enumerate()
        .map(|(i, r)| run_line(r, i == current, frame, g))
        .collect();
    let title = format!("Runs ({})", runs.len());
    f.render_widget(
        Paragraph::new(lines).block(bordered_focused(&title, focus == Focus::Runs)),
        area,
    );
}

fn run_line(r: &RunSummary, is_current: bool, frame: u64, g: Glyphs) -> Line<'static> {
    let (glyph, color) = run_status_glyph(r.status, frame, g);
    let marker = if is_current {
        g.current().to_string()
    } else {
        " ".to_string()
    };
    let label = short_run_label(&r.run_id);
    let mut style = Style::default();
    if is_current {
        style = style.add_modifier(Modifier::BOLD);
    }
    Line::from(vec![
        Span::styled(format!("{marker}{glyph} "), Style::default().fg(color)),
        Span::styled(label, style),
        Span::styled(format!("  {}/{}", r.done, r.total), dim()),
    ])
}

/// Short, stable label for a run id — the trailing random suffix, which is
/// what disambiguates same-minute runs (`2026-07-01-0707-hq27zr` → `hq27zr`).
fn short_run_label(run_id: &str) -> String {
    run_id.rsplit('-').next().unwrap_or(run_id).to_string()
}

fn render_header(f: &mut Frame, area: Rect, h: &HeaderInfo, g: Glyphs) {
    let (status_label, status_color) = run_status_style(h.status);
    let mut counts = vec![
        Span::styled(format!("{}", h.done), Style::default().fg(Color::Green)),
        Span::raw(format!("/{} done", h.total)),
    ];
    if h.running > 0 {
        counts.push(Span::raw(" · "));
        counts.push(Span::styled(
            format!("{}{} running", h.running, g.running()),
            Style::default().fg(Color::Cyan),
        ));
    }
    if h.failed > 0 {
        counts.push(Span::raw(" · "));
        counts.push(Span::styled(
            format!("{}{} failed", h.failed, g.failed()),
            Style::default().fg(Color::Red),
        ));
    }
    if h.blocked > 0 {
        counts.push(Span::raw(" · "));
        counts.push(Span::styled(
            format!("{}{} blocked", h.blocked, g.blocked()),
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

    // Spend now lives in the meters row (sparkline title), so the header
    // meta line carries the git anchors and elapsed clock.
    let mut meta = vec![
        Span::styled("elapsed ", dim()),
        Span::raw(h.elapsed_secs.map(fmt_dur).unwrap_or_else(|| "—".into())),
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

/// The meters row: a progress gauge (with ETA + spend-rate in its label) on
/// the left, a cumulative-spend sparkline on the right.
fn render_meters(f: &mut Frame, area: Rect, h: &HeaderInfo, spend: &[u64]) {
    let cols =
        Layout::horizontal([Constraint::Percentage(64), Constraint::Percentage(36)]).split(area);

    let ratio = if h.total > 0 {
        (h.done as f64 / h.total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let gauge = Gauge::default()
        .block(bordered("Progress"))
        .gauge_style(Style::default().fg(Color::Green))
        .ratio(ratio)
        .label(progress_label(h, ratio));
    f.render_widget(gauge, cols[0]);

    // Sparkline of cumulative spend. `max` pins the top so the curve doesn't
    // rescale every tick; an empty history just draws a flat baseline.
    let max = spend.iter().copied().max().unwrap_or(1).max(1);
    let title = format!("spend ${:.2}", h.usd);
    let spark = Sparkline::default()
        .block(bordered(&title))
        .max(max)
        .style(Style::default().fg(Color::Cyan))
        .data(spend);
    f.render_widget(spark, cols[1]);
}

/// The gauge's inline label: `3/16 · 19% · ETA 12m · $0.08/min`. ETA and rate
/// are only shown once there's enough signal to estimate them.
fn progress_label(h: &HeaderInfo, ratio: f64) -> String {
    let mut s = format!(
        "{}/{} · {}%",
        h.done,
        h.total,
        (ratio * 100.0).round() as u32
    );
    if let Some(eta) = eta_secs(h) {
        s.push_str(&format!(" · ETA {}", fmt_dur(eta)));
    }
    if let Some(rate) = spend_per_min(h) {
        s.push_str(&format!(" · ${rate:.2}/min"));
    }
    s
}

/// Linear ETA from average time-per-completed-task, or `None` when it can't
/// be estimated yet (nothing done, already finished, or no elapsed clock).
fn eta_secs(h: &HeaderInfo) -> Option<i64> {
    let elapsed = h.elapsed_secs?;
    if h.done == 0 || h.done >= h.total || elapsed <= 0 {
        return None;
    }
    let remaining = (h.total - h.done) as i64;
    Some((elapsed * remaining) / h.done as i64)
}

/// Spend rate in USD per minute, or `None` before any elapsed time / spend.
fn spend_per_min(h: &HeaderInfo) -> Option<f64> {
    let elapsed = h.elapsed_secs?;
    if elapsed <= 0 || h.usd <= 0.0 {
        return None;
    }
    Some(h.usd * 60.0 / elapsed as f64)
}

fn render_tasks(
    f: &mut Frame,
    area: Rect,
    tasks: &[TaskRow],
    selected: usize,
    frame: u64,
    focused: bool,
    g: Glyphs,
) -> u16 {
    let lines: Vec<Line> = tasks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let line = task_line(t, frame, g);
            // Highlight the selection: a solid bar when the pane has focus,
            // an underline otherwise so you can still see where you are.
            if i == selected {
                let hl = if focused {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default().add_modifier(Modifier::UNDERLINED)
                };
                line.patch_style(hl)
            } else {
                line
            }
        })
        .collect();

    // Scroll so the selected row stays visible.
    let inner_h = area.height.saturating_sub(2) as usize;
    let scroll = scroll_to_reveal(selected, tasks.len(), inner_h);
    let title = format!("Tasks ({})", tasks.len());
    let p = Paragraph::new(lines)
        .block(bordered_focused(&title, focused))
        .scroll((scroll, 0));
    f.render_widget(p, area);
    scroll
}

/// Vertical scroll offset that keeps row `selected` within a window of
/// `height` rows out of `total`.
fn scroll_to_reveal(selected: usize, total: usize, height: usize) -> u16 {
    if height == 0 || total <= height {
        return 0;
    }
    let max = (total - height) as u16;
    let off = if selected < height {
        0
    } else {
        (selected + 1 - height) as u16
    };
    off.min(max)
}

fn render_agents(f: &mut Frame, area: Rect, agents: &[AgentRow], frame: u64, g: Glyphs) {
    let lines: Vec<Line> = if agents.is_empty() {
        vec![Line::from(Span::styled("(no agents yet)", dim()))]
    } else {
        agents.iter().map(|a| agent_line(a, frame, g)).collect()
    };
    let title = format!("Agents ({})", agents.len());
    f.render_widget(Paragraph::new(lines).block(bordered(&title)), area);
}

fn render_log(f: &mut Frame, area: Rect, log: &[LogRow], view: &mut LogView, focused: bool) {
    // Apply the severity + text filters, then render the surviving rows.
    let rows: Vec<&LogRow> = log.iter().filter(|r| view.accepts(r)).collect();
    let lines: Vec<Line> = rows.iter().map(|&r| log_line(r)).collect();

    let inner_h = area.height.saturating_sub(2);
    let max = (lines.len() as u16).saturating_sub(inner_h);
    // Follow-mode sticks to the newest events; otherwise honour the user's
    // scroll, clamped so we never page past the ends.
    let scroll = if view.follow {
        max
    } else {
        view.scroll.min(max)
    };
    view.scroll = scroll;

    let title = view.title(rows.len(), log.len());
    f.render_widget(
        Paragraph::new(lines)
            .block(bordered_focused(&title, focused))
            .scroll((scroll, 0)),
        area,
    );
}

fn render_footer(
    f: &mut Frame,
    area: Rect,
    finished: bool,
    show_runs: bool,
    awaiting: bool,
    toast: Option<&str>,
) {
    // A control-action toast takes over the footer line when present.
    if let Some(t) = toast {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" ✓ {t} "),
                Style::default().fg(Color::Green),
            ))),
            area,
        );
        return;
    }
    // While a run is parked at the approval gate, foreground the a/v hint.
    if awaiting {
        let spans = vec![
            Span::styled(
                " plan awaiting approval — ",
                Style::default().fg(Color::Yellow),
            ),
            Span::styled("a", Style::default().fg(Color::Green)),
            Span::styled(" approve · ", dim()),
            Span::styled("v", Style::default().fg(Color::Red)),
            Span::styled(" veto · ", dim()),
            Span::styled("q", Style::default().fg(Color::Cyan)),
            Span::styled(" quit ", dim()),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let cyan = Style::default().fg(Color::Cyan);
    let key = |k: &str, desc: &str| {
        [
            Span::styled(k.to_string(), cyan),
            Span::styled(format!(" {desc} · "), dim()),
        ]
    };
    let mut spans = vec![Span::raw(" ")];
    spans.extend(key("Tab", "focus"));
    spans.extend(key("↑/↓", "scroll"));
    spans.extend(key("↵", "detail"));
    if show_runs {
        spans.extend(key("1-9", "run"));
    }
    spans.extend(key("/", "search"));
    spans.extend(key("x", "abort"));
    spans.extend(key("?", "help"));
    spans.push(Span::styled("q", cyan));
    spans.push(Span::styled(" quit ", dim()));

    // With a sidebar the switch hints stay useful even after the watched
    // run finishes; only the single-run view collapses to the exit note.
    let hint = if finished && !show_runs {
        Line::from(Span::styled(
            " run finished — press q to exit ",
            Style::default().fg(Color::Green),
        ))
    } else {
        Line::from(spans)
    };
    f.render_widget(Paragraph::new(hint), area);
}

/// Floating overlay with the full detail for one task and the worker running
/// it — the fields the one-line row has to truncate, plus that task's recent
/// log lines.
fn render_detail(f: &mut Frame, area: Rect, model: &DashboardModel, id: &str, g: Glyphs) {
    let rect = centered_rect(72, 74, area);
    f.render_widget(Clear, rect);

    let Some(t) = model.tasks.iter().find(|t| t.id == id) else {
        let p = Paragraph::new(format!("task {id} is no longer present"))
            .block(bordered_focused("Detail", true));
        f.render_widget(p, rect);
        return;
    };
    // The worker assigned to this task, matched by its friendly name.
    let agent = t
        .agent
        .as_ref()
        .and_then(|nm| model.agents.iter().find(|a| &a.name == nm));

    let label =
        |k: &str, v: String| Line::from(vec![Span::styled(format!("{k:<9}"), dim()), Span::raw(v)]);
    let mut lines: Vec<Line> = Vec::new();

    let (glyph, color) = task_status_style(t.status, g);
    lines.push(Line::from(vec![
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
        Span::styled(
            format!("{} ", t.id),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("[{}] ", t.role), dim()),
        Span::styled(format!("{:?}", t.status), Style::default().fg(color)),
    ]));
    lines.push(Line::raw(t.title.clone()));
    lines.push(Line::raw(""));

    if !t.deps.is_empty() {
        lines.push(label("deps", t.deps.join(", ")));
    }
    lines.push(label("writes", format!("{}", t.writes)));
    lines.push(label("attempts", format!("{}", t.attempts)));
    if t.usd > 0.0 {
        lines.push(label("spend", format!("${:.4}", t.usd)));
    }
    if let Some(secs) = t.elapsed_secs {
        lines.push(label("elapsed", fmt_dur(secs)));
    }

    // Assigned worker.
    lines.push(Line::raw(""));
    if let Some(a) = agent {
        let name_color = agent_color(&a.name);
        lines.push(Line::from(vec![
            Span::styled("worker   ", dim()),
            Span::styled(
                a.name.clone(),
                Style::default().fg(name_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  [{}] {:?}", a.role, a.status), dim()),
        ]));
        if let Some(tool) = &a.tool {
            lines.push(label("tool", format!("{}{tool}", g.tool())));
        }
        if let Some(pid) = a.pid {
            lines.push(label("pid", format!("{pid}")));
        }
        if let Some(secs) = a.uptime_secs {
            lines.push(label("uptime", fmt_dur(secs)));
        }
    } else {
        lines.push(label("worker", "unassigned".into()));
    }

    // This task's recent log lines (by id or worker name).
    let hits: Vec<&LogRow> = model
        .log
        .iter()
        .filter(|r| {
            r.text.contains(&t.id) || t.agent.as_ref().is_some_and(|nm| r.text.contains(nm))
        })
        .collect();
    if !hits.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled("recent", dim()));
        for r in hits.iter().rev().take(8).rev() {
            lines.push(log_line(r));
        }
    }

    let title = format!("Task {} — Esc to close", t.id);
    let p = Paragraph::new(lines)
        .block(bordered_focused(&title, true))
        .wrap(Wrap { trim: false });
    f.render_widget(p, rect);
}

/// The `?` keybinding cheat-sheet overlay.
fn render_help(f: &mut Frame, area: Rect, g: Glyphs) {
    let rect = centered_rect(58, 72, area);
    f.render_widget(Clear, rect);

    let ud = if g.ascii { "Up/Dn" } else { "↑/↓" };
    let row = |keys: String, desc: &str| {
        Line::from(vec![
            Span::styled(format!("  {keys:<16}"), Style::default().fg(Color::Cyan)),
            Span::raw(desc.to_string()),
        ])
    };
    let lines = vec![
        Line::raw(""),
        row(format!("{ud} · j/k"), "scroll / select in the focused pane"),
        row("PgUp/PgDn".into(), "page the focused pane"),
        row(
            "Home/g · End/G".into(),
            "jump to top / bottom (End re-follows log)",
        ),
        row("Tab".into(), "cycle focus: Tasks · Log · Runs"),
        row("1-9".into(), "jump straight to a run"),
        row("Enter".into(), "open the selected task's detail"),
        row("/".into(), "search the log (Esc clears)"),
        row("f".into(), "cycle log severity: all · warn+ · errors"),
        row("mouse".into(), "wheel scrolls · click selects"),
        Line::raw(""),
        row("x".into(), "abort the run (confirm)"),
        row("r".into(), "retry the selected task (confirm)"),
        row(
            "a / v".into(),
            "approve / veto (while awaiting the plan gate)",
        ),
        Line::raw(""),
        row("?".into(), "toggle this help"),
        row("q · Esc · Ctrl-C".into(), "quit"),
    ];
    let p = Paragraph::new(lines)
        .block(bordered_focused("Keys — press any key to close", true))
        .wrap(Wrap { trim: false });
    f.render_widget(p, rect);
}

/// A rectangle centred in `area`, sized to the given width/height percentages.
fn centered_rect(pct_w: u16, pct_h: u16, area: Rect) -> Rect {
    let v = Layout::vertical([
        Constraint::Percentage((100 - pct_h) / 2),
        Constraint::Percentage(pct_h),
        Constraint::Percentage((100 - pct_h) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_w) / 2),
        Constraint::Percentage(pct_w),
        Constraint::Percentage((100 - pct_w) / 2),
    ])
    .split(v[1])[1]
}

// ---------------------------------------------------------------------------
// Row → styled Line
// ---------------------------------------------------------------------------

fn task_line(t: &TaskRow, frame: u64, g: Glyphs) -> Line<'static> {
    // In-progress rows get an animated circular spinner so the currently
    // worked task is obvious at a glance; everything else keeps its glyph.
    let (glyph, color) = match t.status {
        TaskStatus::InProgress => (g.spinner(frame), task_status_style(t.status, g).1),
        other => task_status_style(other, g),
    };
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
        meta.push(format!("{}{}", g.writes(), t.writes));
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

fn agent_line(a: &AgentRow, frame: u64, g: Glyphs) -> Line<'static> {
    // Working agents animate the same spinner as their in-progress task.
    let (glyph, color) = match a.status {
        AgentStatus::InProgress => (g.spinner(frame), agent_status_style(a.status, g).1),
        other => agent_status_style(other, g),
    };
    // The friendly name gets a stable per-agent colour (hashed from the
    // name) so the same worker is easy to track at a glance.
    let name_color = agent_color(&a.name);
    let mut spans = vec![
        Span::styled(format!(" {glyph} "), Style::default().fg(color)),
        Span::styled(
            format!("{} ", a.name),
            Style::default().fg(name_color).add_modifier(Modifier::BOLD),
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
        meta.push(format!("{}{tool}", g.tool()));
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

/// Stable colour for an agent, hashed from its name. Red/yellow/green are
/// left out of the palette so agent colours don't read as status signals.
fn agent_color(name: &str) -> Color {
    const PALETTE: [Color; 6] = [
        Color::Cyan,
        Color::Magenta,
        Color::Blue,
        Color::LightCyan,
        Color::LightMagenta,
        Color::LightBlue,
    ];
    // Tiny FNV-1a so the mapping is stable across frames and processes.
    let mut h: u32 = 0x811c_9dc5;
    for b in name.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    PALETTE[(h as usize) % PALETTE.len()]
}

fn bordered(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
}

/// Like [`bordered`], but a focused pane gets a brighter cyan border so it's
/// clear which pane the arrow keys are driving.
fn bordered_focused(title: &str, focused: bool) -> Block<'static> {
    let block = bordered(title);
    if focused {
        block.border_style(Style::default().fg(Color::Cyan))
    } else {
        block
    }
}

/// Status glyph + colour for a run in the sidebar. Non-terminal runs animate
/// the spinner; terminal runs get a static mark.
fn run_status_glyph(s: RunStatus, frame: u64, g: Glyphs) -> (char, Color) {
    match s {
        RunStatus::Planning => (g.spinner(frame), Color::Blue),
        RunStatus::AwaitingApproval => (g.spinner(frame), Color::Yellow),
        RunStatus::Running => (g.spinner(frame), Color::Cyan),
        RunStatus::Merging => (g.spinner(frame), Color::Magenta),
        RunStatus::Done => (g.pick('✓', 'v'), Color::Green),
        RunStatus::Failed => (g.pick('✗', 'x'), Color::Red),
        RunStatus::Aborted => (g.pick('⊘', '#'), Color::Yellow),
    }
}

fn task_status_style(s: TaskStatus, g: Glyphs) -> (char, Color) {
    match s {
        TaskStatus::Pending => (g.pick('·', '.'), Color::DarkGray),
        TaskStatus::Todo => (g.pick('○', 'o'), Color::Blue),
        TaskStatus::InProgress => (g.pick('↻', '~'), Color::Cyan),
        TaskStatus::Review => (g.pick('◇', '?'), Color::Magenta),
        TaskStatus::Done => (g.pick('✓', 'v'), Color::Green),
        TaskStatus::Failed => (g.pick('✗', 'x'), Color::Red),
        TaskStatus::Blocked => (g.pick('‼', '!'), Color::Yellow),
    }
}

fn agent_status_style(s: AgentStatus, g: Glyphs) -> (char, Color) {
    match s {
        AgentStatus::Idle => (g.pick('·', '.'), Color::DarkGray),
        AgentStatus::InProgress => (g.pick('↻', '~'), Color::Cyan),
        AgentStatus::Done => (g.pick('✓', 'v'), Color::Green),
        AgentStatus::Failed => (g.pick('✗', 'x'), Color::Red),
        AgentStatus::Aborted => (g.pick('⊘', '#'), Color::Yellow),
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

/// Ring the terminal bell (BEL) unless `ARCCODE_NO_BELL` is set. Writing the
/// control byte is safe under raw mode — it doesn't disturb the cursor.
fn emit_bell() {
    if std::env::var_os("ARCCODE_NO_BELL").is_some() {
        return;
    }
    use std::io::Write;
    let mut out = io::stdout();
    let _ = out.write_all(b"\x07");
    let _ = out.flush();
}

fn setup() -> Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

fn teardown(terminal: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNI: Glyphs = Glyphs { ascii: false };
    const ASC: Glyphs = Glyphs { ascii: true };

    #[test]
    fn spinner_cycles_through_four_circular_frames() {
        let frames: Vec<char> = (0..4).map(|f| UNI.spinner(f)).collect();
        assert_eq!(frames, vec!['◐', '◓', '◑', '◒']);
        // Wraps around and stays in-bounds for large ticks.
        assert_eq!(UNI.spinner(4), UNI.spinner(0));
        assert_eq!(UNI.spinner(4_000_001), UNI.spinner(1));
    }

    #[test]
    fn ascii_glyphs_avoid_non_ascii_codepoints() {
        // Every glyph the ASCII theme can emit must be plain 7-bit ASCII, so
        // a legacy console never renders a tofu box.
        for f in 0..4 {
            assert!(ASC.spinner(f).is_ascii(), "spinner frame {f} not ascii");
        }
        let mut glyphs = vec![
            ASC.current(),
            ASC.tool(),
            ASC.writes(),
            ASC.running(),
            ASC.failed(),
            ASC.blocked(),
        ];
        for s in [
            TaskStatus::Pending,
            TaskStatus::Todo,
            TaskStatus::InProgress,
            TaskStatus::Review,
            TaskStatus::Done,
            TaskStatus::Failed,
            TaskStatus::Blocked,
        ] {
            glyphs.push(task_status_style(s, ASC).0);
        }
        for s in [
            AgentStatus::Idle,
            AgentStatus::InProgress,
            AgentStatus::Done,
            AgentStatus::Failed,
            AgentStatus::Aborted,
        ] {
            glyphs.push(agent_status_style(s, ASC).0);
        }
        for s in [
            RunStatus::Running,
            RunStatus::Done,
            RunStatus::Failed,
            RunStatus::Aborted,
        ] {
            glyphs.push(run_status_glyph(s, 0, ASC).0);
        }
        for c in glyphs {
            assert!(c.is_ascii(), "ASCII theme emitted non-ascii glyph: {c:?}");
        }
    }

    #[test]
    fn ascii_theme_reaches_the_rendered_grid() {
        let runs = vec![
            summary("2026-07-01-0707-hq27zr", RunStatus::Running),
            summary("2026-07-01-0709-a1b2c3", RunStatus::Running),
        ];
        let mut ui = WatchUi::new(runs, 0, ASC);
        ui.model = Some(sample_model());
        let s = render_to_string(&mut ui, 120, 30);
        // The unicode current-run marker / spinner must not appear anywhere.
        for bad in ['▸', '◐', '◓', '◑', '◒', '✓', '✗', '↻'] {
            assert!(!s.contains(bad), "ascii render leaked {bad:?}:\n{s}");
        }
        assert!(s.contains('>'), "ascii current-run marker missing:\n{s}");
    }

    fn summary(id: &str, status: RunStatus) -> RunSummary {
        RunSummary {
            run_id: id.into(),
            dir: std::path::PathBuf::from(id),
            status,
            goal: String::new(),
            done: 0,
            total: 0,
        }
    }

    fn sample_model() -> DashboardModel {
        DashboardModel {
            header: HeaderInfo {
                run_id: "2026-07-01-0707-hq27zr".into(),
                status: RunStatus::Running,
                done: 0,
                running: 1,
                failed: 0,
                blocked: 0,
                total: 1,
                usd: 0.0,
                elapsed_secs: Some(10),
                branch: "b".into(),
                base_short: "abc".into(),
            },
            tasks: vec![],
            agents: vec![],
            log: vec![],
        }
    }

    fn render_to_string(ui: &mut WatchUi, w: u16, h: u16) -> String {
        use ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, ui)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn sidebar_shows_and_lists_runs_when_multiple_active() {
        let runs = vec![
            summary("2026-07-01-0707-hq27zr", RunStatus::Running),
            summary("2026-07-01-0709-a1b2c3", RunStatus::Running),
        ];
        let mut ui = WatchUi::new(runs, 0, UNI);
        ui.model = Some(sample_model());
        let s = render_to_string(&mut ui, 120, 30);
        assert!(s.contains("Runs (2)"), "sidebar title missing:\n{s}");
        assert!(s.contains("hq27zr"), "run 1 label missing");
        assert!(s.contains("a1b2c3"), "run 2 label missing");
    }

    #[test]
    fn sidebar_hidden_for_a_single_run() {
        let mut ui = WatchUi::new(vec![summary("only", RunStatus::Running)], 0, UNI);
        ui.model = Some(sample_model());
        let s = render_to_string(&mut ui, 120, 30);
        assert!(!s.contains("Runs ("), "sidebar should be hidden:\n{s}");
    }

    #[test]
    fn short_run_label_is_the_trailing_suffix() {
        assert_eq!(short_run_label("2026-07-01-0707-hq27zr"), "hq27zr");
        assert_eq!(short_run_label("nodashes"), "nodashes");
    }

    #[test]
    fn active_plus_filters_to_non_terminal_runs() {
        let all = vec![
            summary("a", RunStatus::Running),
            summary("b", RunStatus::Done),
            summary("c", RunStatus::Planning),
        ];
        let list = active_plus(all, None);
        let ids: Vec<&str> = list.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "c"], "finished run 'b' filtered out");
    }

    #[test]
    fn active_plus_keeps_the_watched_run_even_if_finished() {
        let all = vec![
            summary("a", RunStatus::Running),
            summary("b", RunStatus::Done),
        ];
        let list = active_plus(all, Some("b"));
        assert!(
            list.iter().any(|r| r.run_id == "b"),
            "watched-but-finished run stays visible"
        );
    }

    #[test]
    fn active_plus_falls_back_to_all_when_none_active() {
        let all = vec![
            summary("a", RunStatus::Done),
            summary("b", RunStatus::Failed),
        ];
        let list = active_plus(all, None);
        assert_eq!(list.len(), 2, "shows everything rather than a blank UI");
    }

    #[test]
    fn run_status_glyph_spins_only_for_active_runs() {
        assert_eq!(
            run_status_glyph(RunStatus::Running, 0, UNI).0,
            UNI.spinner(0)
        );
        assert_eq!(run_status_glyph(RunStatus::Done, 0, UNI).0, '✓');
        assert_eq!(run_status_glyph(RunStatus::Failed, 0, UNI).0, '✗');
    }

    fn log(text: &str, severity: LogSeverity) -> LogRow {
        LogRow {
            text: text.into(),
            severity,
        }
    }

    #[test]
    fn severity_filter_cycles_all_warn_error() {
        let f = SevFilter::All;
        assert_eq!(f.next(), SevFilter::Warn);
        assert_eq!(f.next().next(), SevFilter::Error);
        assert_eq!(f.next().next().next(), SevFilter::All);

        assert!(SevFilter::All.accepts(LogSeverity::Info));
        assert!(!SevFilter::Warn.accepts(LogSeverity::Info));
        assert!(SevFilter::Warn.accepts(LogSeverity::Warn));
        assert!(SevFilter::Warn.accepts(LogSeverity::Error));
        assert!(!SevFilter::Error.accepts(LogSeverity::Warn));
        assert!(SevFilter::Error.accepts(LogSeverity::Error));
    }

    #[test]
    fn log_view_query_filters_case_insensitively() {
        let mut v = LogView::new();
        let rows = [
            log("task.tool  t2 [brave_otter] edit_file", LogSeverity::Info),
            log("task.status t1 → Failed", LogSeverity::Error),
        ];
        // No query → everything passes.
        assert!(rows.iter().all(|r| v.accepts(r)));
        // Query matches the agent name regardless of case.
        v.query = "BRAVE".into();
        assert!(v.accepts(&rows[0]));
        assert!(!v.accepts(&rows[1]));
    }

    #[test]
    fn log_view_combines_severity_and_query() {
        let mut v = LogView::new();
        v.severity = SevFilter::Error;
        v.query = "t1".into();
        assert!(v.accepts(&log("task.status t1 → Failed", LogSeverity::Error)));
        // Right task, wrong severity.
        assert!(!v.accepts(&log("task.assign t1 → x", LogSeverity::Info)));
    }

    #[test]
    fn log_title_shows_filter_and_pause_state() {
        let mut v = LogView::new();
        assert_eq!(v.title(5, 5), "Live log");
        v.severity = SevFilter::Error;
        v.query = "otter".into();
        v.follow = false;
        let t = v.title(2, 5);
        assert!(t.contains("[errors]"), "{t}");
        assert!(t.contains("/otter"), "{t}");
        assert!(t.contains("2/5"), "{t}");
        assert!(t.contains("(paused)"), "{t}");
    }

    #[test]
    fn cycle_focus_includes_log_and_skips_hidden_runs() {
        // Single run: Tasks → Log → Tasks (Runs pane hidden).
        let mut ui = WatchUi::new(vec![summary("only", RunStatus::Running)], 0, UNI);
        assert_eq!(ui.focus, Focus::Tasks);
        ui.cycle_focus();
        assert_eq!(ui.focus, Focus::Log);
        ui.cycle_focus();
        assert_eq!(ui.focus, Focus::Tasks);

        // Two runs: Tasks → Log → Runs → Tasks.
        let mut ui = WatchUi::new(
            vec![
                summary("a", RunStatus::Running),
                summary("b", RunStatus::Running),
            ],
            0,
            UNI,
        );
        ui.cycle_focus();
        assert_eq!(ui.focus, Focus::Log);
        ui.cycle_focus();
        assert_eq!(ui.focus, Focus::Runs);
        ui.cycle_focus();
        assert_eq!(ui.focus, Focus::Tasks);
    }

    fn header(done: usize, total: usize, elapsed: Option<i64>, usd: f64) -> HeaderInfo {
        HeaderInfo {
            run_id: "r".into(),
            status: RunStatus::Running,
            done,
            running: 1,
            failed: 0,
            blocked: 0,
            total,
            usd,
            elapsed_secs: elapsed,
            branch: "b".into(),
            base_short: "abc".into(),
        }
    }

    #[test]
    fn hit_testing_maps_points_and_rows() {
        let r = Rect::new(0, 5, 20, 6); // border at y=5 and y=10, content 6..10
        assert!(contains(r, 0, 5));
        assert!(!contains(r, 20, 5)); // just past the right edge
        assert!(!contains(r, 0, 11));
        // Border rows have no content row; first content row is index 0.
        assert_eq!(row_in(r, 5), None);
        assert_eq!(row_in(r, 6), Some(0));
        assert_eq!(row_in(r, 9), Some(3));
        assert_eq!(row_in(r, 10), None); // bottom border
    }

    #[test]
    fn pane_at_prefers_runs_then_tasks_then_log() {
        let hit = HitAreas {
            runs: Some(Rect::new(0, 0, 10, 10)),
            tasks: Some(Rect::new(10, 0, 30, 10)),
            log: Some(Rect::new(0, 10, 40, 5)),
            tasks_scroll: 0,
        };
        assert_eq!(hit.pane_at(3, 3), Some(Focus::Runs));
        assert_eq!(hit.pane_at(20, 3), Some(Focus::Tasks));
        assert_eq!(hit.pane_at(5, 12), Some(Focus::Log));
        assert_eq!(hit.pane_at(100, 100), None);
    }

    #[test]
    fn click_on_task_row_selects_it_with_scroll_offset() {
        let mut ui = WatchUi::new(vec![summary("only", RunStatus::Running)], 0, UNI);
        ui.tasks_len = 50;
        // Tasks pane spans rows 5..15 (border at 5); the list is scrolled by 8.
        ui.hit = HitAreas {
            runs: None,
            tasks: Some(Rect::new(0, 5, 40, 10)),
            log: None,
            tasks_scroll: 8,
        };
        // Click content row 2 → index 8 + 2 = 10.
        ui.on_click(3, 8);
        assert_eq!(ui.focus, Focus::Tasks);
        assert_eq!(ui.tasks_sel, 10);
    }

    #[test]
    fn wheel_over_log_scrolls_it_without_focus_change() {
        let mut ui = WatchUi::new(vec![summary("only", RunStatus::Running)], 0, UNI);
        ui.hit = HitAreas {
            runs: None,
            tasks: Some(Rect::new(0, 0, 40, 10)),
            log: Some(Rect::new(0, 10, 40, 6)),
            tasks_scroll: 0,
        };
        assert!(ui.log.follow);
        ui.on_scroll(false, 5, 12); // wheel up over the log
        assert!(!ui.log.follow, "wheel over log detaches follow");
        assert_eq!(ui.focus, Focus::Tasks, "scrolling doesn't steal focus");
    }

    #[test]
    fn eta_is_linear_and_guards_edges() {
        // 2 of 16 done in 120s → 14 remaining × 60s each = 840s.
        assert_eq!(eta_secs(&header(2, 16, Some(120), 0.0)), Some(840));
        // Nothing done yet → no estimate.
        assert_eq!(eta_secs(&header(0, 16, Some(120), 0.0)), None);
        // Already complete → no estimate.
        assert_eq!(eta_secs(&header(16, 16, Some(120), 0.0)), None);
        // No clock → no estimate.
        assert_eq!(eta_secs(&header(2, 16, None, 0.0)), None);
    }

    #[test]
    fn spend_rate_is_per_minute() {
        // $0.42 over 120s → $0.21/min.
        let r = spend_per_min(&header(2, 16, Some(120), 0.42)).unwrap();
        assert!((r - 0.21).abs() < 1e-9, "got {r}");
        assert_eq!(spend_per_min(&header(2, 16, Some(0), 0.42)), None);
        assert_eq!(spend_per_min(&header(2, 16, Some(120), 0.0)), None);
    }

    #[test]
    fn progress_label_summarises_the_run() {
        let l = progress_label(&header(2, 16, Some(120), 0.42), 2.0 / 16.0);
        assert!(l.contains("2/16"), "{l}");
        assert!(l.contains("13%"), "{l}"); // 0.125 rounds to 13
        assert!(l.contains("ETA 14m"), "{l}");
        assert!(l.contains("$0.21/min"), "{l}");
    }

    #[test]
    fn spend_samples_stay_bounded_and_scale_to_cents() {
        let mut ui = WatchUi::new(vec![summary("only", RunStatus::Running)], 0, UNI);
        ui.push_spend_sample(0.42);
        assert_eq!(ui.spend_samples.last(), Some(&42));
        for _ in 0..SPEND_SAMPLES_MAX + 50 {
            ui.push_spend_sample(1.0);
        }
        assert_eq!(ui.spend_samples.len(), SPEND_SAMPLES_MAX);
    }

    #[test]
    fn scroll_to_reveal_keeps_selection_in_window() {
        // Fits entirely → never scrolls.
        assert_eq!(scroll_to_reveal(4, 5, 10), 0);
        // Selection above the fold → no scroll.
        assert_eq!(scroll_to_reveal(2, 100, 10), 0);
        // Selection past the window → scroll so it's the last visible row.
        assert_eq!(scroll_to_reveal(12, 100, 10), 3);
        // Never scrolls past the end.
        assert_eq!(scroll_to_reveal(99, 100, 10), 90);
    }

    fn task(id: &str, agent: Option<&str>) -> TaskRow {
        TaskRow {
            id: id.into(),
            role: "developer".into(),
            title: "Wire up the editor".into(),
            status: TaskStatus::InProgress,
            deps: vec!["t1".into()],
            writes: 3,
            usd: 0.05,
            elapsed_secs: Some(80),
            attempts: 2,
            agent: agent.map(|s| s.into()),
        }
    }

    fn agent_row(name: &str) -> AgentRow {
        AgentRow {
            id: "agent-0006".into(),
            name: name.into(),
            role: "developer".into(),
            status: AgentStatus::InProgress,
            task: Some("t2".into()),
            pid: Some(19572),
            tool: Some("edit_file".into()),
            uptime_secs: Some(140),
            usd: 0.05,
        }
    }

    #[test]
    fn detail_overlay_shows_task_and_worker() {
        let mut model = sample_model();
        model.tasks = vec![task("t2", Some("brave_otter"))];
        model.agents = vec![agent_row("brave_otter")];
        model.log = vec![log(
            "task.tool t2 [brave_otter] edit_file",
            LogSeverity::Info,
        )];

        let mut ui = WatchUi::new(vec![summary("only", RunStatus::Running)], 0, UNI);
        ui.model = Some(model);
        ui.detail = Some("t2".into());
        let s = render_to_string(&mut ui, 120, 30);
        assert!(s.contains("Task t2"), "overlay title missing:\n{s}");
        assert!(s.contains("brave_otter"), "worker name missing:\n{s}");
        assert!(s.contains("Wire up the editor"), "full title missing:\n{s}");
        assert!(s.contains("edit_file"), "recent log line missing:\n{s}");
    }

    #[test]
    fn describe_cmd_is_human_readable() {
        assert_eq!(describe_cmd(&ControlCommand::AbortRun), "run abort sent");
        assert_eq!(
            describe_cmd(&ControlCommand::RetryTask { id: "t4".into() }),
            "retry sent for t4"
        );
        assert_eq!(describe_cmd(&ControlCommand::Approve), "approved");
    }

    #[test]
    fn awaiting_approval_tracks_header_status() {
        let mut ui = WatchUi::new(vec![summary("only", RunStatus::AwaitingApproval)], 0, UNI);
        let mut model = sample_model();
        model.header.status = RunStatus::AwaitingApproval;
        ui.model = Some(model);
        assert!(ui.awaiting_approval());
        ui.model.as_mut().unwrap().header.status = RunStatus::Running;
        assert!(!ui.awaiting_approval());
    }

    #[test]
    fn send_control_writes_to_the_run_channel() {
        let dir = tempfile::tempdir().unwrap();
        let run = RunSummary {
            run_id: "r1".into(),
            dir: dir.path().to_path_buf(),
            status: RunStatus::Running,
            goal: String::new(),
            done: 0,
            total: 0,
        };
        let mut ui = WatchUi::new(vec![run], 0, UNI);
        ui.send_control(ControlCommand::AbortRun, "run abort sent");
        assert_eq!(ui.toast.as_deref(), Some("run abort sent"));

        // The command lands in the run's control.jsonl.
        let mut reader = control::ControlReader::new();
        assert_eq!(reader.poll(dir.path()), vec![ControlCommand::AbortRun]);
    }

    #[test]
    fn confirm_overlay_shows_the_prompt() {
        let mut ui = WatchUi::new(vec![summary("only", RunStatus::Running)], 0, UNI);
        ui.model = Some(sample_model());
        ui.confirm = Some(Confirm {
            prompt: "Abort this run? (y/n)".into(),
            cmd: ControlCommand::AbortRun,
        });
        let s = render_to_string(&mut ui, 120, 30);
        assert!(s.contains("Confirm"), "confirm title missing:\n{s}");
        assert!(s.contains("Abort this run?"), "prompt missing:\n{s}");
    }

    #[test]
    fn bell_fires_once_per_new_failure_and_on_finish() {
        let mut ui = WatchUi::new(vec![summary("only", RunStatus::Running)], 0, UNI);
        // No change → no bell.
        ui.note_bell_events(0, false);
        assert!(!ui.take_bell());
        // First failure → bell.
        ui.note_bell_events(1, false);
        assert!(ui.take_bell());
        // Same failure count → no repeat.
        ui.note_bell_events(1, false);
        assert!(!ui.take_bell());
        // Another failure → bell again.
        ui.note_bell_events(2, false);
        assert!(ui.take_bell());
        // Finish → bell once, then never again.
        ui.note_bell_events(2, true);
        assert!(ui.take_bell());
        ui.note_bell_events(2, true);
        assert!(!ui.take_bell());
    }

    #[test]
    fn help_overlay_lists_keys_and_wins_over_detail() {
        let mut ui = WatchUi::new(vec![summary("only", RunStatus::Running)], 0, UNI);
        ui.model = Some(sample_model());
        ui.help = true;
        ui.detail = Some("t2".into()); // help takes precedence
        let s = render_to_string(&mut ui, 120, 30);
        assert!(s.contains("Keys"), "help title missing:\n{s}");
        assert!(s.contains("cycle focus"), "focus help missing:\n{s}");
        assert!(
            !s.contains("Task t2"),
            "detail should be hidden behind help:\n{s}"
        );
    }

    #[test]
    fn enter_opens_selected_task_detail() {
        let mut model = sample_model();
        model.tasks = vec![task("t1", None), task("t2", Some("brave_otter"))];
        let mut ui = WatchUi::new(vec![summary("only", RunStatus::Running)], 0, UNI);
        ui.tasks_len = 2;
        ui.model = Some(model);
        ui.tasks_sel = 1;
        assert_eq!(ui.selected_task_id().as_deref(), Some("t2"));
    }

    #[test]
    fn log_nav_detaches_follow_and_end_reattaches() {
        let mut ui = WatchUi::new(vec![summary("only", RunStatus::Running)], 0, UNI);
        ui.focus = Focus::Log;
        assert!(ui.log.follow, "log follows by default");
        ui.nav_up();
        assert!(!ui.log.follow, "scrolling up detaches follow");
        ui.nav_end();
        assert!(ui.log.follow, "End re-attaches follow");
    }

    #[test]
    fn agent_colour_is_stable_and_excludes_status_hues() {
        // Deterministic across calls…
        assert_eq!(agent_color("brave_otter"), agent_color("brave_otter"));
        // …and never red/green/yellow, which are reserved for status.
        for name in ["brave_otter", "lucid_lynx", "swift_heron", "calm_panda"] {
            let c = agent_color(name);
            assert!(
                !matches!(c, Color::Red | Color::Green | Color::Yellow),
                "{name} got a status-reserved colour: {c:?}"
            );
        }
    }
}
