//! Session picker for /resume.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Widget},
};

use super::{centered_rect, ModalOutcome};

#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub path: PathBuf,
    /// Display label (timestamp extracted from filename).
    pub label: String,
    pub provider: String,
    pub model: String,
}

#[derive(Debug)]
pub struct SessionPicker {
    sessions: Vec<SessionEntry>,
    selected: usize,
}

impl SessionPicker {
    pub fn new(sessions: Vec<SessionEntry>) -> Self {
        Self { sessions, selected: 0 }
    }

    pub fn take_selected(&mut self) -> Option<SessionEntry> {
        self.sessions.get(self.selected).cloned()
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        match k.code {
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            KeyCode::Down => {
                if self.selected + 1 < self.sessions.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Enter => {
                if self.sessions.is_empty() {
                    return ModalOutcome::Continue;
                }
                return ModalOutcome::Close;
            }
            _ => {}
        }
        ModalOutcome::Continue
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let rect = centered_rect(area, 70, 60);
        Clear.render(rect, buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " Resume Session ",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(rect);
        block.render(rect, buf);

        if self.sessions.is_empty() {
            Paragraph::new("No saved sessions found.")
                .render(inner, buf);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(inner);

        let items: Vec<ListItem> = self.sessions.iter().enumerate().map(|(i, s)| {
            let marker = if i == self.selected { "› " } else { "  " };
            let style = if i == self.selected {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let label = format!("{}{} ({}/{})", marker, s.label, s.provider, s.model);
            ListItem::new(Line::from(Span::styled(label, style)))
        }).collect();
        List::new(items).render(chunks[0], buf);

        Paragraph::new(Line::from(Span::styled(
            "↑/↓ navigate · Enter resume · Esc cancel",
            Style::default().fg(Color::DarkGray),
        ))).render(chunks[1], buf);
    }
}
