//! Diff preview modal: shown before write_file / edit_file commits changes.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap},
};

use super::{centered_rect, ModalOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffDecision {
    Approved,
    Rejected,
}

#[derive(Debug)]
pub struct DiffPreviewModal {
    pub diff: String,
    pub path: String,
    pub decision: Option<DiffDecision>,
    scroll: u16,
}

impl DiffPreviewModal {
    pub fn new(path: impl Into<String>, diff: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            diff: diff.into(),
            decision: None,
            scroll: 0,
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                self.decision = Some(DiffDecision::Approved);
                return ModalOutcome::Close;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.decision = Some(DiffDecision::Rejected);
                return ModalOutcome::Close;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1);
            }
            _ => {}
        }
        ModalOutcome::Continue
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let rect = centered_rect(area, 85, 85);
        Clear.render(rect, buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(Span::styled(
                format!(" Diff preview: {} ", self.path),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(rect);
        block.render(rect, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(inner);

        // Render diff with colored lines
        let lines: Vec<Line> = self.diff.lines().map(|line| {
            if line.starts_with('+') && !line.starts_with("+++") {
                Line::from(Span::styled(line.to_string(), Style::default().fg(Color::Green)))
            } else if line.starts_with('-') && !line.starts_with("---") {
                Line::from(Span::styled(line.to_string(), Style::default().fg(Color::Red)))
            } else if line.starts_with("@@") {
                Line::from(Span::styled(line.to_string(), Style::default().fg(Color::Cyan)))
            } else {
                Line::from(Span::raw(line.to_string()))
            }
        }).collect();

        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0))
            .render(chunks[0], buf);

        Paragraph::new(Line::from(vec![
            Span::styled(" y ", Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw(" Apply  "),
            Span::styled(" n ", Style::default().fg(Color::Black).bg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw(" Reject  "),
            Span::styled(" ↑↓ ", Style::default().fg(Color::DarkGray)),
            Span::raw("Scroll"),
        ]))
        .render(chunks[1], buf);
    }
}
