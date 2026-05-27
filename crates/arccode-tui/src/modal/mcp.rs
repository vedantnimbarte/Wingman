//! `/mcp` — browse, add, connect, disconnect, remove MCP servers.
//!
//! Combines list + small "add" sub-form in a single modal. Async work
//! (connect/add/remove/disconnect) is dispatched back to the host via
//! [`take_pending_task`]; on completion the host refreshes the list with
//! [`set_servers`].

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Row, Table, Widget},
};

use super::{centered_rect, ModalOutcome};

/// View-friendly snapshot of one MCP server. Mirrors the CLI's
/// `McpServerView` but is defined here so this crate doesn't need to know
/// about `arccode_mcp` types directly.
#[derive(Debug, Clone)]
pub struct McpServerSummary {
    pub name: String,
    pub command: String,
    pub connected: bool,
    pub tool_count: usize,
}

/// Stripped-down add-form payload sent back to the host. Host converts it
/// into an `arccode_config::McpServerConfig`.
#[derive(Debug, Clone)]
pub struct McpAddPayload {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum McpTask {
    Add(McpAddPayload),
    Remove(String),
    Connect(String),
    Disconnect(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Add,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddField {
    Name,
    Command,
    Args,
}

#[derive(Debug)]
pub struct McpView {
    servers: Vec<McpServerSummary>,
    selected: usize,
    mode: Mode,
    add_field: AddField,
    name: String,
    command: String,
    args: String,
    /// Set after an async task; cleared on the next keypress.
    feedback: Option<String>,
    pending: Option<McpTask>,
    /// Set while a task is in flight so input is ignored.
    in_flight: bool,
}

impl McpView {
    pub fn new(servers: Vec<McpServerSummary>) -> Self {
        Self {
            servers,
            selected: 0,
            mode: Mode::List,
            add_field: AddField::Name,
            name: String::new(),
            command: String::new(),
            args: String::new(),
            feedback: None,
            pending: None,
            in_flight: false,
        }
    }

    pub fn set_servers(&mut self, servers: Vec<McpServerSummary>) {
        if self.selected >= servers.len() && !servers.is_empty() {
            self.selected = servers.len() - 1;
        }
        if servers.is_empty() {
            self.selected = 0;
        }
        self.servers = servers;
    }

    pub fn take_pending_task(&mut self) -> Option<McpTask> {
        let t = self.pending.take();
        if t.is_some() {
            self.in_flight = true;
        }
        t
    }

    pub fn task_completed(&mut self, result: Result<(), String>) {
        self.in_flight = false;
        match result {
            Ok(()) => {
                self.feedback = Some("ok".into());
                // Return to the list view after a successful add.
                if self.mode == Mode::Add {
                    self.mode = Mode::List;
                    self.name.clear();
                    self.command.clear();
                    self.args.clear();
                    self.add_field = AddField::Name;
                }
            }
            Err(e) => self.feedback = Some(e),
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        if self.in_flight {
            return ModalOutcome::Continue;
        }
        self.feedback = None;
        match self.mode {
            Mode::List => self.handle_list(k),
            Mode::Add => self.handle_add(k),
        }
    }

    fn handle_list(&mut self, k: KeyEvent) -> ModalOutcome {
        match k.code {
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            KeyCode::Down => {
                if self.selected + 1 < self.servers.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.mode = Mode::Add;
                self.add_field = AddField::Name;
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                if let Some(s) = self.servers.get(self.selected) {
                    self.pending = Some(McpTask::Connect(s.name.clone()));
                }
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                if let Some(s) = self.servers.get(self.selected) {
                    self.pending = Some(McpTask::Disconnect(s.name.clone()));
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                if let Some(s) = self.servers.get(self.selected) {
                    self.pending = Some(McpTask::Remove(s.name.clone()));
                }
            }
            _ => {}
        }
        ModalOutcome::Continue
    }

    fn handle_add(&mut self, k: KeyEvent) -> ModalOutcome {
        let buf = match self.add_field {
            AddField::Name => &mut self.name,
            AddField::Command => &mut self.command,
            AddField::Args => &mut self.args,
        };
        match k.code {
            KeyCode::Char(c) => buf.push(c),
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Tab | KeyCode::Down => {
                self.add_field = next_field(self.add_field);
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.add_field = prev_field(self.add_field);
            }
            KeyCode::Enter => match self.add_field {
                AddField::Name => self.add_field = AddField::Command,
                AddField::Command => self.add_field = AddField::Args,
                AddField::Args => {
                    if self.name.trim().is_empty() {
                        self.feedback = Some("name required".into());
                        self.add_field = AddField::Name;
                    } else if self.command.trim().is_empty() {
                        self.feedback = Some("command required".into());
                        self.add_field = AddField::Command;
                    } else {
                        self.pending = Some(McpTask::Add(McpAddPayload {
                            name: self.name.trim().to_string(),
                            command: self.command.trim().to_string(),
                            args: self
                                .args
                                .split_whitespace()
                                .map(str::to_string)
                                .collect(),
                        }));
                    }
                }
            },
            KeyCode::Esc => {
                // Esc inside the form returns to the list, not close-modal.
                // The host's outer Esc handler still closes the modal when
                // we're in List mode.
                self.mode = Mode::List;
            }
            _ => {}
        }
        ModalOutcome::Continue
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let rect = centered_rect(area, 82, 70);
        Clear.render(rect, buf);
        let title = match self.mode {
            Mode::List => " /mcp — MCP servers ",
            Mode::Add => " /mcp add — new server ",
        };
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = outer.inner(rect);
        outer.render(rect, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),    // body
                Constraint::Length(2), // feedback / hint
            ])
            .split(inner);
        match self.mode {
            Mode::List => self.render_list(chunks[0], buf),
            Mode::Add => self.render_add(chunks[0], buf),
        }
        self.render_footer(chunks[1], buf);
    }

    fn render_list(&self, area: Rect, buf: &mut Buffer) {
        if self.servers.is_empty() {
            Paragraph::new(Line::from(Span::styled(
                "(no MCP servers configured — press 'n' to add one)",
                Style::default().fg(Color::DarkGray),
            )))
            .render(area, buf);
            return;
        }
        let header = Row::new(vec!["name", "status", "tools", "command"]).style(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::UNDERLINED),
        );
        let rows: Vec<Row> = self
            .servers
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let status = if s.connected { "● live" } else { "○ idle" };
                let style = if i == self.selected {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Row::new(vec![
                    s.name.clone(),
                    status.to_string(),
                    s.tool_count.to_string(),
                    s.command.clone(),
                ])
                .style(style)
            })
            .collect();
        Table::new(
            rows,
            [
                Constraint::Percentage(22),
                Constraint::Percentage(12),
                Constraint::Percentage(10),
                Constraint::Percentage(56),
            ],
        )
        .header(header)
        .render(area, buf);
    }

    fn render_add(&self, area: Rect, buf: &mut Buffer) {
        let mk = |label: &str, value: &str, field: AddField, masked: bool| {
            let active = field == self.add_field;
            let arrow = if active { "› " } else { "  " };
            let label_style = if active {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let value_display = if masked && !value.is_empty() {
                "*".repeat(value.chars().count())
            } else {
                value.to_string()
            };
            let value_style = Style::default().fg(Color::White);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{arrow}{label:<10}"), label_style),
                Span::styled(value_display, value_style),
                if active {
                    Span::raw("▏")
                } else {
                    Span::raw("")
                },
            ]))
        };
        let items = vec![
            mk("name", &self.name, AddField::Name, false),
            mk("command", &self.command, AddField::Command, false),
            mk("args", &self.args, AddField::Args, false),
        ];
        List::new(items).render(area, buf);
    }

    fn render_footer(&self, area: Rect, buf: &mut Buffer) {
        let line = if let Some(f) = &self.feedback {
            Line::from(vec![
                Span::styled("· ", Style::default().fg(Color::Yellow)),
                Span::styled(f.clone(), Style::default().fg(Color::Yellow)),
            ])
        } else if self.in_flight {
            Line::from(Span::styled(
                "(working…)",
                Style::default().fg(Color::Yellow),
            ))
        } else {
            let hint = match self.mode {
                Mode::List => "↑/↓ select · n new · c connect · d disconnect · r remove · Esc close",
                Mode::Add => "Tab/↑↓ next field · Enter advance / submit · Esc back",
            };
            Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray)))
        };
        Paragraph::new(line).render(area, buf);
    }
}

fn next_field(f: AddField) -> AddField {
    match f {
        AddField::Name => AddField::Command,
        AddField::Command => AddField::Args,
        AddField::Args => AddField::Name,
    }
}

fn prev_field(f: AddField) -> AddField {
    match f {
        AddField::Name => AddField::Args,
        AddField::Command => AddField::Name,
        AddField::Args => AddField::Command,
    }
}
