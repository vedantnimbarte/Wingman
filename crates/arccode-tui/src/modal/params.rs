//! Model parameter editor modal.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};

use super::{centered_rect, ModalOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Temperature,
    MaxTokens,
}

#[derive(Debug)]
pub struct ParamsModal {
    pub temperature: String,   // String so user can type a float
    pub max_tokens: String,    // String so user can type an int
    active_field: Field,
    pub committed: bool,
}

impl ParamsModal {
    pub fn new(temperature: Option<f32>, max_tokens: u32) -> Self {
        Self {
            temperature: temperature.map(|t| format!("{t:.2}")).unwrap_or_default(),
            max_tokens: max_tokens.to_string(),
            active_field: Field::Temperature,
            committed: false,
        }
    }

    /// Returns parsed values if the user committed (pressed Enter on the last field / Ctrl+S).
    pub fn take_result(&self) -> Option<(Option<f32>, u32)> {
        if !self.committed {
            return None;
        }
        let temp = if self.temperature.is_empty() {
            None
        } else {
            self.temperature.parse::<f32>().ok()
        };
        let max_tok = self.max_tokens.parse::<u32>().unwrap_or(4096);
        Some((temp, max_tok))
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        match k.code {
            KeyCode::Tab | KeyCode::Down => {
                self.active_field = match self.active_field {
                    Field::Temperature => Field::MaxTokens,
                    Field::MaxTokens => Field::Temperature,
                };
            }
            KeyCode::Up => {
                self.active_field = match self.active_field {
                    Field::Temperature => Field::MaxTokens,
                    Field::MaxTokens => Field::Temperature,
                };
            }
            KeyCode::Enter => {
                if self.active_field == Field::MaxTokens {
                    self.committed = true;
                    return ModalOutcome::Close;
                }
                self.active_field = Field::MaxTokens;
            }
            KeyCode::Backspace => {
                match self.active_field {
                    Field::Temperature => { self.temperature.pop(); }
                    Field::MaxTokens => { self.max_tokens.pop(); }
                }
            }
            KeyCode::Char(c) => {
                match self.active_field {
                    Field::Temperature => self.temperature.push(c),
                    Field::MaxTokens => self.max_tokens.push(c),
                }
            }
            _ => {}
        }
        ModalOutcome::Continue
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let rect = centered_rect(area, 50, 30);
        Clear.render(rect, buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Magenta))
            .title(Span::styled(
                " Model Parameters ",
                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(rect);
        block.render(rect, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);

        let temp_style = if self.active_field == Field::Temperature {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        Paragraph::new(Line::from(Span::styled("temperature (0.0–1.0, blank = default):", Style::default().fg(Color::DarkGray))))
            .render(chunks[0], buf);
        Paragraph::new(Line::from(vec![
            Span::styled(&self.temperature, temp_style),
            Span::raw("▏"),
        ]))
        .render(chunks[1], buf);

        let tok_style = if self.active_field == Field::MaxTokens {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        Paragraph::new(Line::from(Span::styled("max_tokens:", Style::default().fg(Color::DarkGray))))
            .render(chunks[2], buf);
        Paragraph::new(Line::from(vec![
            Span::styled(&self.max_tokens, tok_style),
            Span::raw("▏"),
        ]))
        .render(chunks[3], buf);

        Paragraph::new(Line::from(Span::styled(
            "Tab/↑↓ switch field · Enter confirm · Esc cancel",
            Style::default().fg(Color::DarkGray),
        )))
        .render(chunks[5], buf);
    }
}
