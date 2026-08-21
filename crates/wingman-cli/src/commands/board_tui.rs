//! Full-screen kanban board for `wingman board`.
//!
//! Five columns across every registered project. The card layout lives in
//! `wingman_board::view` (headless and snapshot-tested); this module draws it,
//! handles keys, and owns the two write paths a board is allowed:
//!
//! - **Card writes** — create, edit, archive, dispatch — go to `board.db`.
//! - **Run writes** — abort only — go through `control::append`, the same
//!   channel `pilot watch` uses. The board never touches run state directly.
//!
//! A card never spends money without a confirmation modal.

use std::collections::HashSet;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use wingman_autonomous::control::{self, ControlCommand};
use wingman_board::view::MIN_GRID_WIDTH;
use wingman_board::{BoardCard, BoardStore, Column, DispatchOpts, NewCard, SubRow};

use super::pilot_ui::{self, Glyphs, Term};

/// Redraw cadence. Reloads are rarer (see `RELOAD_EVERY`) because the
/// expensive part — reading `state.json` — is already mtime-gated by the
/// roll-up cache.
const TICK: Duration = Duration::from_millis(250);
const RELOAD_EVERY: u64 = 4;

/// One navigable row: a card, or one of its tasks when expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    Card(usize),
    Sub(usize, usize),
}

/// What a text prompt is collecting. New-card entry is two-stage, so the
/// title rides along in the variant until the goal arrives.
#[derive(Debug, Clone, PartialEq)]
enum Prompt {
    Search,
    NewTitle,
    NewGoal { title: String },
    EditTitle { card: String },
    EditGoal { card: String },
}

impl Prompt {
    fn label(&self) -> &'static str {
        match self {
            Prompt::Search => "search",
            Prompt::NewTitle => "new card title",
            Prompt::NewGoal { .. } => "goal (blank = use the title)",
            Prompt::EditTitle { .. } => "title",
            Prompt::EditGoal { .. } => "goal",
        }
    }
}

/// A pending action awaiting y/n. Only the two that cost something — money or
/// in-flight work — get one.
#[derive(Debug, Clone, PartialEq)]
enum Action {
    Dispatch { card: String },
    Abort { run_dir: std::path::PathBuf },
}

#[derive(Debug, Clone, PartialEq)]
struct Confirm {
    prompt: String,
    action: Action,
}

/// Which badge filter is active. Cycled with `f`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BadgeFilter {
    All,
    Failed,
    Blocked,
}

impl BadgeFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Failed,
            Self::Failed => Self::Blocked,
            Self::Blocked => Self::All,
        }
    }

    fn label(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Failed => Some("has-failed"),
            Self::Blocked => Some("has-blocked"),
        }
    }

    fn accepts(self, c: &BoardCard) -> bool {
        let r = c.rollup.as_ref();
        match self {
            Self::All => true,
            Self::Failed => r.is_some_and(|r| r.failed > 0),
            Self::Blocked => r.is_some_and(|r| r.blocked > 0),
        }
    }
}

struct BoardUi {
    store: BoardStore,
    /// Every card, before filtering.
    all: Vec<BoardCard>,
    /// Cards per column after filtering — what the UI indexes.
    columns: Vec<Vec<BoardCard>>,
    /// Flattened navigable rows per column: cards and their tasks.
    entries: Vec<Vec<Entry>>,
    col: usize,
    row: usize,
    expanded: HashSet<String>,
    project_filter: Option<String>,
    projects: Vec<String>,
    badge: BadgeFilter,
    query: String,
    /// Open text prompt, if any. Swallows most keys while set.
    prompt: Option<Prompt>,
    input: String,
    confirm: Option<Confirm>,
    /// `(card id, task id)` of the open task detail.
    detail: Option<(String, String)>,
    help: bool,
    toast: Option<(String, Instant)>,
    glyphs: Glyphs,
    frame: u64,
    error: Option<String>,
    /// Set when `o` wants the caller to hand off to `pilot watch`.
    handoff: Option<(std::path::PathBuf, String)>,
}

impl BoardUi {
    fn new(store: BoardStore, ascii: bool) -> Self {
        let projects = store
            .projects(false)
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.id)
            .collect();
        let mut ui = BoardUi {
            store,
            all: Vec::new(),
            columns: vec![Vec::new(); Column::ALL.len()],
            entries: vec![Vec::new(); Column::ALL.len()],
            col: 0,
            row: 0,
            expanded: HashSet::new(),
            project_filter: None,
            projects,
            badge: BadgeFilter::All,
            query: String::new(),
            prompt: None,
            input: String::new(),
            confirm: None,
            detail: None,
            help: false,
            toast: None,
            glyphs: Glyphs { ascii },
            frame: 0,
            error: None,
            handoff: None,
        };
        ui.reload();
        ui
    }

    fn reload(&mut self) {
        match self.store.board(self.project_filter.as_deref()) {
            Ok(cards) => {
                self.all = cards;
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        self.refilter();
    }

    /// Rebuild the per-column view and the flattened entry list from `all`,
    /// keeping the selection pointing at something real.
    fn refilter(&mut self) {
        let q = self.query.to_ascii_lowercase();
        let badge = self.badge;
        let matches = |c: &BoardCard| {
            if !badge.accepts(c) {
                return false;
            }
            if q.is_empty() {
                return true;
            }
            c.card.title.to_ascii_lowercase().contains(&q)
                || c.project_name.to_ascii_lowercase().contains(&q)
                || c.card.labels.iter().any(|l| l.contains(&q))
                || c.rollup.as_ref().is_some_and(|r| {
                    r.subrows.iter().any(|s| {
                        s.agent_name
                            .as_deref()
                            .is_some_and(|a| a.to_ascii_lowercase().contains(&q))
                    })
                })
        };

        // Remember what was selected so the cursor survives a reload.
        let anchor = self.selected_card().map(|c| c.card.id.clone());

        self.columns = Column::ALL
            .iter()
            .map(|&col| {
                self.all
                    .iter()
                    .filter(|c| c.column == col && matches(c))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect();

        self.entries = self
            .columns
            .iter()
            .map(|cards| {
                let mut out = Vec::new();
                for (i, c) in cards.iter().enumerate() {
                    out.push(Entry::Card(i));
                    if self.expanded.contains(&c.card.id) {
                        if let Some(r) = &c.rollup {
                            for j in 0..r.subrows.len() {
                                out.push(Entry::Sub(i, j));
                            }
                        }
                    }
                }
                out
            })
            .collect();

        self.restore_selection(anchor);
    }

    /// Put the cursor back on `anchor` if it still exists, else clamp.
    fn restore_selection(&mut self, anchor: Option<String>) {
        if let Some(id) = anchor {
            for (ci, cards) in self.columns.iter().enumerate() {
                if let Some(idx) = cards.iter().position(|c| c.card.id == id) {
                    if let Some(row) = self.entries[ci]
                        .iter()
                        .position(|e| matches!(e, Entry::Card(i) if *i == idx))
                    {
                        self.col = ci;
                        self.row = row;
                        return;
                    }
                }
            }
        }
        self.clamp();
    }

    fn clamp(&mut self) {
        self.col = self.col.min(Column::ALL.len() - 1);
        let len = self.entries[self.col].len();
        self.row = if len == 0 { 0 } else { self.row.min(len - 1) };
    }

    fn entry(&self) -> Option<Entry> {
        self.entries.get(self.col)?.get(self.row).copied()
    }

    /// The card under the cursor — the owning card when a task is selected.
    fn selected_card(&self) -> Option<&BoardCard> {
        let (Entry::Card(i) | Entry::Sub(i, _)) = self.entry()?;
        self.columns[self.col].get(i)
    }

    fn selected_sub(&self) -> Option<&SubRow> {
        let Entry::Sub(i, j) = self.entry()? else {
            return None;
        };
        self.columns[self.col][i].rollup.as_ref()?.subrows.get(j)
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    /// Move to the next column that has rows, so arrowing across an empty
    /// column doesn't strand the cursor on nothing.
    fn move_col(&mut self, delta: isize) {
        let n = Column::ALL.len() as isize;
        let mut i = self.col as isize;
        for _ in 0..n {
            i = (i + delta).rem_euclid(n);
            if !self.entries[i as usize].is_empty() {
                self.col = i as usize;
                self.row = 0;
                return;
            }
        }
    }

    fn cycle_project(&mut self) {
        if self.projects.is_empty() {
            return;
        }
        self.project_filter = match &self.project_filter {
            None => Some(self.projects[0].clone()),
            Some(cur) => {
                let i = self.projects.iter().position(|p| p == cur).unwrap_or(0);
                self.projects.get(i + 1).cloned()
            }
        };
        let label = self.project_filter.clone().unwrap_or_else(|| "all".into());
        self.toast(format!("project: {label}"));
        self.reload();
    }

    /// `Enter`: expand/collapse a card, or open a task's detail.
    fn activate(&mut self) {
        match self.entry() {
            Some(Entry::Card(_)) => self.toggle_expand(),
            Some(Entry::Sub(..)) => {
                let card = self.selected_card().map(|c| c.card.id.clone());
                let task = self.selected_sub().map(|s| s.task_id.clone());
                if let (Some(card), Some(task)) = (card, task) {
                    self.detail = Some((card, task));
                }
            }
            None => {}
        }
    }

    fn toggle_expand(&mut self) {
        let Some(c) = self.selected_card() else {
            return;
        };
        let id = c.card.id.clone();
        if c.rollup.is_none() {
            self.toast("card has no run to expand");
            return;
        }
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        self.refilter();
    }

    // ---- card writes ------------------------------------------------------

    fn open_prompt(&mut self, p: Prompt, seed: &str) {
        self.input = seed.to_string();
        self.prompt = Some(p);
    }

    /// Commit whatever the open prompt was collecting.
    fn submit_prompt(&mut self) {
        let Some(p) = self.prompt.take() else { return };
        let text = self.input.trim().to_string();
        self.input.clear();

        match p {
            Prompt::Search => {}
            Prompt::NewTitle => {
                if text.is_empty() {
                    self.toast("cancelled — a card needs a title");
                    return;
                }
                // Stage two: ask for the goal, carrying the title along.
                self.open_prompt(Prompt::NewGoal { title: text }, "");
            }
            Prompt::NewGoal { title } => self.create(title, text),
            Prompt::EditTitle { card } => {
                if text.is_empty() {
                    self.toast("cancelled — a card needs a title");
                    return;
                }
                self.apply_edit(&card, Some(&text), None);
            }
            Prompt::EditGoal { card } => self.apply_edit(&card, None, Some(&text)),
        }
    }

    fn create(&mut self, title: String, goal: String) {
        let Some(project_id) = self.target_project() else {
            self.toast("no project registered — run a pilot command in a repo first");
            return;
        };
        let goal = (!goal.is_empty()).then_some(goal);
        match self.store.create_card(NewCard {
            project_id,
            title,
            goal,
            ..Default::default()
        }) {
            Ok(c) => {
                let (id, short) = (c.id.clone(), c.short().to_string());
                self.toast(format!("added {short}"));
                self.reload();
                self.restore_selection(Some(id));
            }
            Err(e) => self.toast(format!("add failed: {e}")),
        }
    }

    fn apply_edit(&mut self, card: &str, title: Option<&str>, goal: Option<&str>) {
        match self.store.update_card(card, title, goal) {
            Ok(()) => {
                self.toast("updated");
                self.reload();
            }
            Err(e) => self.toast(format!("edit failed: {e}")),
        }
    }

    /// Which project a new card belongs to: the one under the cursor, else
    /// the active filter, else the first registered.
    fn target_project(&self) -> Option<String> {
        self.selected_card()
            .map(|c| c.card.project_id.clone())
            .or_else(|| self.project_filter.clone())
            .or_else(|| self.projects.first().cloned())
    }

    fn archive_selected(&mut self) {
        let Some(c) = self.selected_card() else {
            return;
        };
        let (id, short) = (c.card.id.clone(), c.card.short().to_string());
        match self.store.set_archived(&id, true) {
            // Archived cards leave the board; `board list --all` still shows
            // them and `board archive <id> --restore` brings them back.
            Ok(()) => {
                self.toast(format!("archived {short} — restore from the CLI"));
                self.reload();
            }
            Err(e) => self.toast(format!("archive failed: {e}")),
        }
    }

    // ---- actions behind a confirmation ------------------------------------

    fn ask_dispatch(&mut self) {
        let Some(c) = self.selected_card() else {
            return;
        };
        if c.project_missing {
            self.toast("project is missing — relocate it from the CLI first");
            return;
        }
        let prompt = format!(
            "Dispatch \"{}\" in {}? This starts a pilot run and spends money.",
            truncate(&c.card.title, 48),
            c.project_name
        );
        self.confirm = Some(Confirm {
            prompt,
            action: Action::Dispatch {
                card: c.card.id.clone(),
            },
        });
    }

    fn ask_abort(&mut self) {
        let Some(c) = self.selected_card() else {
            return;
        };
        let live = c.run_id.is_some() && c.rollup.as_ref().is_some_and(|r| !r.is_terminal());
        if !live {
            self.toast("no live run on this card");
            return;
        }
        let card_id = c.card.id.clone();
        match self.store.newest_dispatch(&card_id) {
            Ok(Some(d)) => {
                self.confirm = Some(Confirm {
                    prompt: format!("Abort run {}? In-flight workers are cancelled.", d.run_id),
                    action: Action::Abort { run_dir: d.run_dir },
                });
            }
            _ => self.toast("could not resolve the run"),
        }
    }

    fn run_confirmed(&mut self, c: Confirm) {
        match c.action {
            Action::Dispatch { card } => {
                match self.dispatch(&card) {
                    Ok(run_id) => self.toast(format!("dispatched -> {run_id}")),
                    Err(e) => self.toast(format!("dispatch failed: {e}")),
                }
                self.reload();
            }
            Action::Abort { run_dir } => {
                match control::append(&run_dir, &ControlCommand::AbortRun) {
                    Ok(()) => self.toast("abort requested"),
                    Err(e) => self.toast(format!("abort failed: {e}")),
                }
            }
        }
    }

    fn dispatch(&self, card_id: &str) -> wingman_board::Result<String> {
        let card = self.store.find_card(card_id)?;
        let project = self.store.project(&card.project_id)?;
        let out = self
            .store
            .dispatch_card(&card, &project, &DispatchOpts::default())?;
        Ok(out.run_id)
    }

    /// Ask the caller to hand off to `pilot watch` for this card's newest run.
    fn request_handoff(&mut self) {
        let Some(c) = self.selected_card() else {
            return;
        };
        let Some(run_id) = c.run_id.clone() else {
            self.toast("card has no run to watch");
            return;
        };
        let project_id = c.card.project_id.clone();
        match self.store.project(&project_id) {
            Ok(p) if p.exists() => self.handoff = Some((p.root, run_id)),
            Ok(_) => self.toast("project is missing on disk"),
            Err(e) => self.toast(e.to_string()),
        }
    }
}

/// Open the board. Blocks until the user quits.
pub async fn run(ascii: bool) -> Result<ExitCode> {
    let store = super::board::open()?;
    let mut ui = BoardUi::new(store, ascii);

    loop {
        let mut terminal = pilot_ui::setup()?;
        let outcome = run_loop(&mut terminal, &mut ui);
        pilot_ui::teardown(&mut terminal)?;
        outcome?;

        // `o` drops out of the loop so the board's terminal is fully restored
        // before `pilot watch` claims it; we re-enter when the user quits it.
        let Some((root, run_id)) = ui.handoff.take() else {
            return Ok(ExitCode::SUCCESS);
        };
        super::pilot_watch_tui::run(&root, Some(run_id), 250, ui.glyphs.ascii)?;
        ui.reload();
    }
}

fn run_loop(terminal: &mut Term, ui: &mut BoardUi) -> Result<ExitCode> {
    loop {
        terminal.draw(|f| draw(f, ui))?;

        if event::poll(TICK)? {
            match event::read()? {
                CtEvent::Key(k) if k.kind == KeyEventKind::Press => {
                    if !handle_key(ui, k.code, k.modifiers) {
                        return Ok(ExitCode::SUCCESS);
                    }
                    if ui.handoff.is_some() {
                        return Ok(ExitCode::SUCCESS);
                    }
                }
                CtEvent::Resize(..) => {}
                _ => {}
            }
        }

        ui.frame += 1;
        // Never reload under an open prompt or modal: it would move the cursor
        // out from under whatever the user is deciding about.
        if ui.frame.is_multiple_of(RELOAD_EVERY)
            && ui.prompt.is_none()
            && ui.confirm.is_none()
            && ui.detail.is_none()
        {
            ui.reload();
        }
        if let Some((_, at)) = &ui.toast {
            if at.elapsed() > Duration::from_secs(3) {
                ui.toast = None;
            }
        }
    }
}

/// Returns false to quit.
fn handle_key(ui: &mut BoardUi, code: KeyCode, mods: KeyModifiers) -> bool {
    // Prompts and modals get first refusal, innermost first.
    if let Some(p) = ui.prompt.clone() {
        match code {
            KeyCode::Esc => {
                ui.prompt = None;
                ui.input.clear();
                if p == Prompt::Search {
                    ui.query.clear();
                    ui.refilter();
                }
            }
            KeyCode::Enter => ui.submit_prompt(),
            KeyCode::Backspace => {
                ui.input.pop();
                if p == Prompt::Search {
                    ui.query.clone_from(&ui.input);
                    ui.refilter();
                }
            }
            KeyCode::Char(c) => {
                ui.input.push(c);
                if p == Prompt::Search {
                    ui.query.clone_from(&ui.input);
                    ui.refilter();
                }
            }
            _ => {}
        }
        return true;
    }

    if let Some(c) = ui.confirm.clone() {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                ui.confirm = None;
                ui.run_confirmed(c);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                ui.confirm = None;
                ui.toast("cancelled");
            }
            _ => {}
        }
        return true;
    }

    if ui.detail.is_some() {
        ui.detail = None;
        return true;
    }

    if ui.help {
        ui.help = false;
        return true;
    }

    match code {
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return false,
        KeyCode::Char('q') | KeyCode::Esc => return false,
        KeyCode::Left | KeyCode::Char('h') => ui.move_col(-1),
        KeyCode::Right | KeyCode::Char('l') => ui.move_col(1),
        KeyCode::Up | KeyCode::Char('k') => ui.row = ui.row.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            let len = ui.entries[ui.col].len();
            if len > 0 {
                ui.row = (ui.row + 1).min(len - 1);
            }
        }
        KeyCode::Home => ui.row = 0,
        KeyCode::End => ui.row = ui.entries[ui.col].len().saturating_sub(1),
        KeyCode::Enter => ui.activate(),
        KeyCode::Char('n') => ui.open_prompt(Prompt::NewTitle, ""),
        KeyCode::Char('e') => {
            if let Some(c) = ui.selected_card() {
                let (id, title) = (c.card.id.clone(), c.card.title.clone());
                ui.open_prompt(Prompt::EditTitle { card: id }, &title);
            }
        }
        KeyCode::Char('g') => {
            if let Some(c) = ui.selected_card() {
                let (id, goal) = (c.card.id.clone(), c.card.goal.clone());
                ui.open_prompt(Prompt::EditGoal { card: id }, &goal);
            }
        }
        KeyCode::Char('d') => ui.ask_dispatch(),
        KeyCode::Char('a') => ui.archive_selected(),
        KeyCode::Char('x') => ui.ask_abort(),
        KeyCode::Char('o') => ui.request_handoff(),
        KeyCode::Char('p') => ui.cycle_project(),
        KeyCode::Char('f') => {
            ui.badge = ui.badge.next();
            let l = ui.badge.label().unwrap_or("all");
            ui.toast(format!("filter: {l}"));
            ui.refilter();
        }
        KeyCode::Char('/') => {
            let seed = ui.query.clone();
            ui.open_prompt(Prompt::Search, &seed);
        }
        KeyCode::Char('r') => {
            if let Err(e) = ui.store.clear_rollup_cache() {
                ui.toast(format!("reload failed: {e}"));
            }
            ui.reload();
            ui.toast("reloaded");
        }
        KeyCode::Char('?') => ui.help = true,
        _ => {}
    }
    true
}

fn draw(f: &mut Frame, ui: &mut BoardUi) {
    let area = f.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(area);

    render_header(f, chunks[0], ui);
    if area.width < MIN_GRID_WIDTH as u16 {
        render_narrow(f, chunks[1], ui);
    } else {
        render_columns(f, chunks[1], ui);
    }
    render_footer(f, chunks[2], ui);

    if let Some((card_id, task_id)) = ui.detail.clone() {
        render_detail(f, area, ui, &card_id, &task_id);
    }
    if let Some(c) = &ui.confirm {
        render_confirm(f, area, c);
    }
    if ui.help {
        render_help(f, area);
    }
}

fn render_header(f: &mut Frame, area: Rect, ui: &BoardUi) {
    let total: usize = ui.columns.iter().map(Vec::len).sum();
    let running = ui.columns[2].len();
    let mut spans = vec![
        Span::styled(
            " wingman board ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  {total} cards  {running} running")),
    ];
    if let Some(p) = &ui.project_filter {
        spans.push(Span::styled(
            format!("  project:{p}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    if let Some(l) = ui.badge.label() {
        spans.push(Span::styled(
            format!("  filter:{l}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    if !ui.query.is_empty() {
        spans.push(Span::styled(
            format!("  /{}", ui.query),
            Style::default().fg(Color::Yellow),
        ));
    }
    if let Some(e) = &ui.error {
        spans.push(Span::styled(
            format!("  {e}"),
            Style::default().fg(Color::Red),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_columns(f: &mut Frame, area: Rect, ui: &BoardUi) {
    let n = Column::ALL.len();
    let cols = Layout::horizontal(vec![Constraint::Ratio(1, n as u32); n]).split(area);

    for (i, &column) in Column::ALL.iter().enumerate() {
        let focused = i == ui.col;
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ({}) ", column.title(), ui.columns[i].len()))
            .border_style(if focused {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            });

        let inner = block.inner(cols[i]);
        f.render_widget(block, cols[i]);
        f.render_widget(
            Paragraph::new(column_lines(ui, i)).wrap(Wrap { trim: false }),
            inner,
        );
    }
}

/// Every line of one column, with the selected entry highlighted.
fn column_lines<'a>(ui: &BoardUi, col: usize) -> Vec<Line<'a>> {
    let focused = col == ui.col;
    let mut lines = Vec::new();
    for (row, entry) in ui.entries[col].iter().enumerate() {
        let sel = focused && row == ui.row;
        match *entry {
            Entry::Card(i) => {
                if i > 0 {
                    lines.push(Line::raw(""));
                }
                lines.extend(card_lines(ui, &ui.columns[col][i], sel));
            }
            Entry::Sub(i, j) => {
                if let Some(r) = &ui.columns[col][i].rollup {
                    let last = j + 1 == r.subrows.len();
                    lines.extend(sub_lines(ui, &r.subrows[j], last, sel));
                }
            }
        }
    }
    lines
}

fn card_lines<'a>(ui: &BoardUi, c: &BoardCard, selected: bool) -> Vec<Line<'a>> {
    let g = ui.glyphs;
    let marker = if c.rollup.is_none() {
        ' '
    } else if ui.expanded.contains(&c.card.id) {
        g.expanded()
    } else {
        g.collapsed()
    };

    let title_style = if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };

    let mut out = vec![
        Line::from(vec![
            Span::raw(format!("{marker} ")),
            Span::styled(c.project_name.clone(), Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(Span::styled(format!("  {}", c.card.title), title_style)),
    ];

    if !c.badges.is_empty() {
        let text = c
            .badges
            .iter()
            .map(|b| b.text())
            .collect::<Vec<_>>()
            .join(" · ");
        out.push(Line::from(Span::styled(
            format!("  {text}"),
            Style::default().fg(badge_colour(c)),
        )));
    }
    out
}

fn sub_lines<'a>(ui: &BoardUi, s: &SubRow, last: bool, selected: bool) -> Vec<Line<'a>> {
    let g = ui.glyphs;
    let branch = if last { g.branch_last() } else { g.branch() };
    let style = if selected {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::default()
    };
    let detail = if s.blocked_by.is_empty() {
        format!(
            "{}  {}  ${:.2}",
            s.agent_name.as_deref().unwrap_or("--"),
            s.model.as_deref().unwrap_or("--"),
            s.usd
        )
    } else {
        format!("dep {}", s.blocked_by.join(","))
    };
    vec![
        Line::from(Span::styled(
            format!(" {branch}{} {}", status_glyph(ui, s.status), s.title),
            style,
        )),
        Line::from(Span::styled(
            format!("    {detail}"),
            Style::default().fg(Color::DarkGray),
        )),
    ]
}

fn status_glyph(ui: &BoardUi, s: wingman_autonomous::TaskStatus) -> char {
    use wingman_autonomous::TaskStatus as T;
    let g = ui.glyphs;
    match s {
        T::Done => g.done(),
        T::Failed => g.failed(),
        T::Blocked => g.blocked(),
        T::InProgress => g.spinner(ui.frame / 2),
        T::Review => g.pick('◆', '?'),
        T::Pending | T::Todo => g.pick('·', '.'),
    }
}

fn badge_colour(c: &BoardCard) -> Color {
    let Some(r) = &c.rollup else {
        return Color::DarkGray;
    };
    if r.failed > 0 {
        Color::Red
    } else if r.blocked > 0 {
        Color::Yellow
    } else {
        Color::DarkGray
    }
}

/// Single-column list for terminals too narrow for five panes.
fn render_narrow(f: &mut Frame, area: Rect, ui: &BoardUi) {
    let mut lines = Vec::new();
    for (i, &column) in Column::ALL.iter().enumerate() {
        if ui.columns[i].is_empty() {
            continue;
        }
        lines.push(Line::from(Span::styled(
            format!("{} ({})", column.title(), ui.columns[i].len()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        lines.extend(column_lines(ui, i));
        lines.push(Line::raw(""));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_footer(f: &mut Frame, area: Rect, ui: &BoardUi) {
    if let Some(p) = &ui.prompt {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {}: {}_", p.label(), ui.input),
                Style::default().fg(Color::Yellow),
            ))),
            area,
        );
        return;
    }
    if let Some((msg, _)) = &ui.toast {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {msg}"),
                Style::default().fg(Color::Green),
            ))),
            area,
        );
        return;
    }
    let hint = " enter open  n new  e title  g goal  d dispatch  a archive  x abort  o watch  p project  / search  ? help  q quit";
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))),
        area,
    );
}

/// Task detail. Built from the board's own `SubRow` rather than the watch
/// TUI's `DashboardModel` — the shapes differ, and `SubRow` carries the one
/// field the watch overlay lacks: the model that ran the task.
fn render_detail(f: &mut Frame, area: Rect, ui: &BoardUi, card_id: &str, task_id: &str) {
    let rect = pilot_ui::centered(72, 74, area);
    f.render_widget(Clear, rect);

    let found = ui
        .all
        .iter()
        .find(|c| c.card.id == card_id)
        .and_then(|c| c.rollup.as_ref())
        .and_then(|r| r.subrows.iter().find(|s| s.task_id == task_id));

    let Some(s) = found else {
        f.render_widget(
            Paragraph::new(format!("task {task_id} is no longer present"))
                .block(Block::default().borders(Borders::ALL).title(" task ")),
            rect,
        );
        return;
    };

    let dim = Style::default().fg(Color::DarkGray);
    let label =
        |k: &str, v: String| Line::from(vec![Span::styled(format!("{k:<10}"), dim), Span::raw(v)]);

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{} ", s.task_id),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("[{}] ", s.role), dim),
            Span::raw(format!("{:?}", s.status)),
        ]),
        Line::raw(s.title.clone()),
        Line::raw(""),
    ];

    if !s.deps.is_empty() {
        lines.push(label("deps", s.deps.join(", ")));
    }
    if !s.blocked_by.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(format!("{:<10}", "blocked by"), dim),
            Span::styled(s.blocked_by.join(", "), Style::default().fg(Color::Yellow)),
        ]));
    }
    lines.push(label("writes", format!("{} path(s)", s.writes)));
    lines.push(label("attempts", s.attempts.to_string()));
    if let Some(secs) = s.elapsed_secs {
        lines.push(label("elapsed", fmt_dur(secs)));
    }
    if s.usd > 0.0 {
        lines.push(label("spend", format!("${:.4}", s.usd)));
    }

    lines.push(Line::raw(""));
    lines.push(label(
        "worker",
        s.agent_name.clone().unwrap_or_else(|| "unassigned".into()),
    ));
    lines.push(label(
        "model",
        s.model.clone().unwrap_or_else(|| "--".into()),
    ));
    if let Some(t) = &s.current_tool {
        lines.push(label("tool", format!("{}{t}", ui.glyphs.tool())));
    }
    if let Some(w) = &s.worktree {
        lines.push(label("worktree", w.clone()));
    }

    if let Some(o) = &s.outcome {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("outcome", dim)));
        lines.push(Line::raw(o.clone()));
    }

    if let Some(sid) = &s.session_id {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("transcript", dim)));
        lines.push(Line::raw(format!("  {sid}")));
        lines.push(Line::from(Span::styled(
            "  .wingman/sessions/<id>.jsonl — `wingman session fork` to branch it",
            dim,
        )));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "any key to close  ·  o opens this run in pilot watch",
        dim,
    )));

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" task "))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn render_confirm(f: &mut Frame, area: Rect, c: &Confirm) {
    let rect = pilot_ui::centered(50, 26, area);
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
            Span::styled(" confirm    ", Style::default().fg(Color::DarkGray)),
            Span::styled("n/Esc", Style::default().fg(Color::Red)),
            Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
        ]),
    ];
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" confirm "))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn render_help(f: &mut Frame, area: Rect) {
    let rect = pilot_ui::centered(62, 78, area);
    f.render_widget(Clear, rect);
    let lines = vec![
        Line::from("wingman board"),
        Line::raw(""),
        Line::from("  arrows / hjkl   move between columns and rows"),
        Line::from("  enter           expand a card, or open a task's detail"),
        Line::raw(""),
        Line::from("  n               new card (title, then goal)"),
        Line::from("  e / g           edit the card's title / goal"),
        Line::from("  d               dispatch — starts a pilot run (confirms)"),
        Line::from("  a               archive the card"),
        Line::from("  x               abort the card's live run (confirms)"),
        Line::from("  o               open the run in pilot watch"),
        Line::raw(""),
        Line::from("  p               cycle the project filter"),
        Line::from("  /               search title, project, label, agent"),
        Line::from("  f               cycle badge filter (failed / blocked)"),
        Line::from("  r               force reload, bypassing the cache"),
        Line::from("  q / esc         quit"),
        Line::raw(""),
        Line::from("Columns are derived from run state, never stored."),
        Line::from("Restore a card with: wingman board archive <id> --restore"),
        Line::raw(""),
        Line::from("press any key to close"),
    ];
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" help "))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn fmt_dur(secs: i64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let (m, s) = (secs / 60, secs % 60);
    if m < 60 {
        return format!("{m}m{s:02}s");
    }
    format!("{}h{:02}m", m / 60, m % 60)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('~');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wingman_autonomous::{RunStatus, TaskStatus};
    use wingman_board::card::Card;
    use wingman_board::column::Badge;
    use wingman_board::rollup::Rollup;

    fn card(title: &str, column: Column) -> BoardCard {
        BoardCard {
            card: Card {
                id: format!("{title}xxxxxxxxxxxx")[..12].to_string(),
                project_id: "p".into(),
                title: title.into(),
                goal: String::new(),
                notes: None,
                labels: vec![],
                ord: 0.0,
                archived: false,
                created_at: String::new(),
                updated_at: String::new(),
            },
            project_name: "Proj".into(),
            project_missing: false,
            run_id: None,
            rollup: None,
            column,
            badges: vec![Badge::Progress { done: 1, total: 2 }],
        }
    }

    fn sub(id: &str, status: TaskStatus) -> SubRow {
        SubRow {
            task_id: id.into(),
            title: format!("task {id}"),
            status,
            role: "developer".into(),
            agent_name: Some("brave_otter".into()),
            model: Some("opus-5".into()),
            session_id: Some("sess-1".into()),
            usd: 0.5,
            attempts: 1,
            blocked_by: vec![],
            current_tool: None,
            deps: vec![],
            writes: 2,
            elapsed_secs: Some(150),
            outcome: Some("did the thing".into()),
            worktree: Some(".wingman/worktrees/x".into()),
        }
    }

    fn with_run(mut c: BoardCard, subs: Vec<SubRow>) -> BoardCard {
        c.run_id = Some("r1".into());
        c.rollup = Some(Rollup {
            status: RunStatus::Running,
            done: 1,
            total: subs.len(),
            failed: 0,
            blocked: 0,
            review: 0,
            usd: 1.0,
            subrows: subs,
        });
        c
    }

    fn ui_with(cards: Vec<BoardCard>) -> BoardUi {
        let dir = tempfile::tempdir().unwrap();
        let store = BoardStore::open(&dir.path().join("b.db")).unwrap();
        // Leak the tempdir so the SQLite file outlives the test body.
        std::mem::forget(dir);
        let mut ui = BoardUi::new(store, true);
        ui.all = cards;
        ui.refilter();
        ui
    }

    // ---- navigation -------------------------------------------------------

    #[test]
    fn move_col_skips_empty_columns() {
        let mut ui = ui_with(vec![card("a", Column::Backlog), card("b", Column::Done)]);
        ui.move_col(1);
        assert_eq!(ui.col, 4);
        ui.move_col(1);
        assert_eq!(ui.col, 0);
    }

    #[test]
    fn move_col_on_an_empty_board_does_not_hang() {
        let mut ui = ui_with(vec![]);
        ui.move_col(1);
        assert_eq!(ui.col, 0);
    }

    #[test]
    fn expanding_adds_navigable_subrows() {
        let c = with_run(
            card("run", Column::InProgress),
            vec![
                sub("t1", TaskStatus::Done),
                sub("t2", TaskStatus::InProgress),
            ],
        );
        let mut ui = ui_with(vec![c]);
        assert_eq!(ui.entries[2].len(), 1, "collapsed: just the card");

        ui.col = 2;
        ui.toggle_expand();
        assert_eq!(ui.entries[2].len(), 3, "card + two tasks");
        assert!(matches!(ui.entries[2][1], Entry::Sub(0, 0)));

        ui.toggle_expand();
        assert_eq!(ui.entries[2].len(), 1, "collapse removes them again");
    }

    #[test]
    fn selected_card_works_from_a_subrow() {
        let c = with_run(
            card("run", Column::InProgress),
            vec![sub("t1", TaskStatus::Done)],
        );
        let mut ui = ui_with(vec![c]);
        ui.col = 2;
        ui.toggle_expand();
        ui.row = 1;

        assert!(matches!(ui.entry(), Some(Entry::Sub(0, 0))));
        assert_eq!(ui.selected_card().unwrap().card.title, "run");
        assert_eq!(ui.selected_sub().unwrap().task_id, "t1");
    }

    #[test]
    fn enter_on_a_subrow_opens_detail_not_collapse() {
        let c = with_run(
            card("run", Column::InProgress),
            vec![sub("t1", TaskStatus::Done)],
        );
        let mut ui = ui_with(vec![c]);
        ui.col = 2;
        ui.activate(); // expand the card
        ui.row = 1;
        ui.activate(); // open the task

        assert!(ui.detail.is_some());
        assert_eq!(ui.detail.as_ref().unwrap().1, "t1");
        // One card + its one task. Collapsing would leave just the card.
        assert_eq!(ui.entries[2].len(), 2, "must not have collapsed the card");
    }

    #[test]
    fn selection_survives_a_reload() {
        let mut ui = ui_with(vec![
            card("alpha", Column::Backlog),
            card("beta", Column::Backlog),
        ]);
        ui.row = 1;
        let want = ui.selected_card().unwrap().card.id.clone();

        ui.refilter();
        assert_eq!(ui.selected_card().unwrap().card.id, want);
    }

    #[test]
    fn filtering_clamps_the_selection() {
        let mut ui = ui_with(vec![
            card("alpha", Column::Backlog),
            card("beta", Column::Backlog),
        ]);
        ui.row = 1;
        ui.query = "alph".into();
        ui.refilter();
        assert_eq!(ui.row, 0);
        assert_eq!(ui.columns[0].len(), 1);
    }

    // ---- prompts and modals ----------------------------------------------

    #[test]
    fn new_card_prompt_is_two_stage() {
        let mut ui = ui_with(vec![]);
        handle_key(&mut ui, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(ui.prompt, Some(Prompt::NewTitle));

        for ch in "Fix it".chars() {
            handle_key(&mut ui, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        handle_key(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            ui.prompt,
            Some(Prompt::NewGoal {
                title: "Fix it".into()
            }),
            "title must carry into stage two"
        );
    }

    #[test]
    fn new_card_with_an_empty_title_is_refused() {
        let mut ui = ui_with(vec![]);
        handle_key(&mut ui, KeyCode::Char('n'), KeyModifiers::NONE);
        handle_key(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        assert!(ui.prompt.is_none());
        assert!(ui.toast.is_some());
    }

    #[test]
    fn new_card_round_trips_into_the_store() {
        let mut ui = ui_with(vec![]);
        // A project must exist for a card to belong to.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let pid = ui.store.touch_project(&root).unwrap();
        ui.projects = vec![pid];

        ui.create("Fix LSP".into(), "stop the storm".into());
        let cards = ui.store.cards(None, false).unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].title, "Fix LSP");
        assert_eq!(cards[0].goal, "stop the storm");
    }

    #[test]
    fn new_card_without_a_project_is_refused() {
        let mut ui = ui_with(vec![]);
        ui.create("orphan".into(), String::new());
        assert!(ui.store.cards(None, true).unwrap().is_empty());
        assert!(ui.toast.is_some());
    }

    #[test]
    fn prompt_keys_do_not_leak_into_navigation() {
        let mut ui = ui_with(vec![card("a", Column::Backlog)]);
        handle_key(&mut ui, KeyCode::Char('/'), KeyModifiers::NONE);
        assert_eq!(ui.prompt, Some(Prompt::Search));

        // `q` must type, not quit; `d` must not dispatch.
        assert!(handle_key(&mut ui, KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(handle_key(&mut ui, KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(ui.input, "qd");
        assert!(ui.confirm.is_none());

        handle_key(&mut ui, KeyCode::Esc, KeyModifiers::NONE);
        assert!(ui.prompt.is_none());
        assert!(ui.query.is_empty());
    }

    #[test]
    fn search_prompt_filters_as_you_type() {
        let mut ui = ui_with(vec![
            card("alpha", Column::Backlog),
            card("beta", Column::Backlog),
        ]);
        handle_key(&mut ui, KeyCode::Char('/'), KeyModifiers::NONE);
        for ch in "alph".chars() {
            handle_key(&mut ui, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert_eq!(ui.columns[0].len(), 1);

        for _ in 0..4 {
            handle_key(&mut ui, KeyCode::Backspace, KeyModifiers::NONE);
        }
        assert_eq!(ui.columns[0].len(), 2, "clearing restores every card");
    }

    #[test]
    fn edit_seeds_the_prompt_with_the_current_value() {
        let mut ui = ui_with(vec![card("alpha", Column::Backlog)]);
        handle_key(&mut ui, KeyCode::Char('e'), KeyModifiers::NONE);
        assert_eq!(ui.input, "alpha", "editing starts from the existing title");
    }

    #[test]
    fn dispatch_asks_before_spending() {
        let mut ui = ui_with(vec![card("a", Column::Backlog)]);
        handle_key(&mut ui, KeyCode::Char('d'), KeyModifiers::NONE);

        let c = ui.confirm.clone().expect("dispatch must confirm first");
        assert!(matches!(c.action, Action::Dispatch { .. }));
        assert!(c.prompt.contains("spends money"));

        handle_key(&mut ui, KeyCode::Char('n'), KeyModifiers::NONE);
        assert!(ui.confirm.is_none());
    }

    #[test]
    fn confirm_swallows_navigation_keys() {
        let mut ui = ui_with(vec![card("a", Column::Backlog), card("b", Column::Done)]);
        handle_key(&mut ui, KeyCode::Char('d'), KeyModifiers::NONE);
        let before = ui.col;

        handle_key(&mut ui, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(ui.col, before, "arrows must not move under a modal");
        assert!(ui.confirm.is_some(), "and must not dismiss it");
    }

    #[test]
    fn dispatch_on_a_missing_project_is_refused() {
        let mut c = card("a", Column::Backlog);
        c.project_missing = true;
        let mut ui = ui_with(vec![c]);
        handle_key(&mut ui, KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(ui.confirm.is_none());
        assert!(ui.toast.is_some());
    }

    #[test]
    fn abort_refuses_when_there_is_no_live_run() {
        let mut ui = ui_with(vec![card("a", Column::Backlog)]);
        handle_key(&mut ui, KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(ui.confirm.is_none());
        assert!(ui.toast.is_some());
    }

    #[test]
    fn abort_on_a_terminal_run_is_refused() {
        let mut c = with_run(
            card("done", Column::Done),
            vec![sub("t1", TaskStatus::Done)],
        );
        c.rollup.as_mut().unwrap().status = RunStatus::Done;
        let mut ui = ui_with(vec![c]);
        ui.col = 4;
        handle_key(&mut ui, KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(ui.confirm.is_none(), "a finished run has nothing to abort");
    }

    #[test]
    fn abort_writes_a_control_command() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("run");
        std::fs::create_dir_all(&run_dir).unwrap();

        let mut ui = ui_with(vec![]);
        ui.run_confirmed(Confirm {
            prompt: "x".into(),
            action: Action::Abort {
                run_dir: run_dir.clone(),
            },
        });

        let body = std::fs::read_to_string(control::control_path(&run_dir)).unwrap();
        assert_eq!(
            ControlCommand::parse(body.trim()),
            Some(ControlCommand::AbortRun)
        );
    }

    #[test]
    fn handoff_needs_a_run() {
        let mut ui = ui_with(vec![card("a", Column::Backlog)]);
        handle_key(&mut ui, KeyCode::Char('o'), KeyModifiers::NONE);
        assert!(ui.handoff.is_none());
        assert!(ui.toast.is_some());
    }

    #[test]
    fn handoff_resolves_the_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();

        let mut ui = ui_with(vec![]);
        let pid = ui.store.touch_project(&root).unwrap();
        let mut c = with_run(
            card("run", Column::InProgress),
            vec![sub("t1", TaskStatus::Done)],
        );
        c.card.project_id = pid;
        ui.all = vec![c];
        ui.refilter();
        ui.col = 2;

        handle_key(&mut ui, KeyCode::Char('o'), KeyModifiers::NONE);
        let (got, run) = ui.handoff.clone().expect("handoff should be requested");
        assert!(got.ends_with("repo"));
        assert_eq!(run, "r1");
    }

    #[test]
    fn archive_removes_the_card_from_the_board() {
        let mut ui = ui_with(vec![]);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let pid = ui.store.touch_project(&root).unwrap();
        ui.projects = vec![pid];
        ui.create("temp".into(), String::new());
        assert_eq!(ui.columns[0].len(), 1);

        ui.archive_selected();
        assert!(ui.columns[0].is_empty(), "archived cards leave the board");
        assert_eq!(
            ui.store.cards(None, true).unwrap().len(),
            1,
            "but not the db"
        );
    }

    #[test]
    fn detail_closes_on_any_key() {
        let c = with_run(
            card("run", Column::InProgress),
            vec![sub("t1", TaskStatus::Done)],
        );
        let mut ui = ui_with(vec![c]);
        ui.col = 2;
        ui.activate();
        ui.row = 1;
        ui.activate();
        assert!(ui.detail.is_some());

        handle_key(&mut ui, KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(ui.detail.is_none());
    }

    #[test]
    fn quit_keys_quit() {
        let mut ui = ui_with(vec![]);
        assert!(!handle_key(&mut ui, KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!handle_key(&mut ui, KeyCode::Esc, KeyModifiers::NONE));
        assert!(!handle_key(
            &mut ui,
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        ));
    }

    #[test]
    fn help_closes_on_any_key() {
        let mut ui = ui_with(vec![]);
        handle_key(&mut ui, KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(ui.help);
        handle_key(&mut ui, KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(!ui.help);
    }

    #[test]
    fn badge_filter_cycles() {
        assert_eq!(BadgeFilter::All.next(), BadgeFilter::Failed);
        assert_eq!(BadgeFilter::Blocked.next(), BadgeFilter::All);
        let c = card("a", Column::Backlog);
        assert!(BadgeFilter::All.accepts(&c));
        assert!(!BadgeFilter::Failed.accepts(&c));
    }

    #[test]
    fn fmt_dur_scales() {
        assert_eq!(fmt_dur(45), "45s");
        assert_eq!(fmt_dur(150), "2m30s");
        assert_eq!(fmt_dur(7320), "2h02m");
    }

    // ---- rendering --------------------------------------------------------

    fn render(ui: &mut BoardUi, w: u16, h: u16) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
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
    fn wide_render_shows_five_columns() {
        let mut ui = ui_with(vec![card("alpha", Column::Backlog)]);
        let s = render(&mut ui, 160, 20);
        for col in Column::ALL {
            assert!(s.contains(col.title()), "missing {}:\n{s}", col.title());
        }
        assert!(s.contains("alpha"));
    }

    #[test]
    fn expanded_render_shows_model_and_agent() {
        let c = with_run(
            card("run", Column::InProgress),
            vec![sub("t1", TaskStatus::Done)],
        );
        let mut ui = ui_with(vec![c]);
        ui.col = 2;
        ui.toggle_expand();
        let s = render(&mut ui, 200, 24);
        assert!(s.contains("brave_otter"), "agent missing:\n{s}");
        assert!(s.contains("opus-5"), "model missing:\n{s}");
    }

    #[test]
    fn detail_overlay_renders_the_task() {
        let c = with_run(
            card("run", Column::InProgress),
            vec![sub("t1", TaskStatus::Done)],
        );
        let mut ui = ui_with(vec![c]);
        ui.col = 2;
        ui.activate();
        ui.row = 1;
        ui.activate();

        let s = render(&mut ui, 120, 30);
        assert!(s.contains("opus-5"), "model missing:\n{s}");
        assert!(s.contains("sess-1"), "transcript missing:\n{s}");
        assert!(s.contains("did the thing"), "outcome missing:\n{s}");
        assert!(s.contains("2m30s"), "elapsed missing:\n{s}");
    }

    #[test]
    fn confirm_overlay_renders() {
        let mut ui = ui_with(vec![card("alpha", Column::Backlog)]);
        handle_key(&mut ui, KeyCode::Char('d'), KeyModifiers::NONE);
        let s = render(&mut ui, 120, 24);
        assert!(s.contains("confirm"), "{s}");
    }

    #[test]
    fn prompt_shows_in_the_footer() {
        let mut ui = ui_with(vec![]);
        handle_key(&mut ui, KeyCode::Char('n'), KeyModifiers::NONE);
        let s = render(&mut ui, 120, 20);
        assert!(s.contains("new card title"), "{s}");
    }

    #[test]
    fn narrow_render_lists_without_panicking() {
        let mut ui = ui_with(vec![card("alpha", Column::Backlog)]);
        let s = render(&mut ui, 60, 20);
        assert!(s.contains("BACKLOG"));
        assert!(s.contains("alpha"));
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let c = with_run(
            card("run", Column::InProgress),
            vec![sub("t1", TaskStatus::Done)],
        );
        let mut ui = ui_with(vec![c]);
        ui.col = 2;
        ui.toggle_expand();
        ui.row = 1;
        ui.activate();
        ui.help = true;
        // Absurd geometry, with every overlay open, must still not crash.
        let _ = render(&mut ui, 10, 3);
        let _ = render(&mut ui, 1, 1);
    }
}
