//! Skill variable input modal.
//!
//! When a skill body contains `{{var_name}}` placeholders, this modal
//! collects a value for each variable before the skill is applied.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};
use std::collections::HashMap;

use super::{centered_rect, ModalOutcome};

#[derive(Debug)]
pub struct SkillVarsModal {
    /// Ordered list of variable names from the skill body.
    vars: Vec<String>,
    /// Current input values, keyed by variable name.
    values: HashMap<String, String>,
    /// Which variable is currently being edited.
    current: usize,
    pub committed: bool,
}

impl SkillVarsModal {
    pub fn new(vars: Vec<String>) -> Self {
        let values = vars.iter().map(|v| (v.clone(), String::new())).collect();
        Self {
            vars,
            values,
            current: 0,
            committed: false,
        }
    }

    /// Returns collected values if committed.
    pub fn take_result(&self) -> Option<HashMap<String, String>> {
        if self.committed {
            Some(self.values.clone())
        } else {
            None
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        let Some(var) = self.vars.get(self.current) else {
            return ModalOutcome::Continue;
        };
        let var = var.clone();
        match k.code {
            KeyCode::Enter => {
                if self.current + 1 < self.vars.len() {
                    self.current += 1;
                } else {
                    self.committed = true;
                    return ModalOutcome::Close;
                }
            }
            KeyCode::Tab => {
                if self.current + 1 < self.vars.len() {
                    self.current += 1;
                }
            }
            KeyCode::Backspace => {
                if let Some(v) = self.values.get_mut(&var) {
                    v.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(v) = self.values.get_mut(&var) {
                    v.push(c);
                }
            }
            _ => {}
        }
        ModalOutcome::Continue
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let height = (self.vars.len() as u16 * 3 + 5).min(area.height.saturating_sub(4));
        let rect = centered_rect(area, 60, 70);
        let rect = Rect { height: height.min(rect.height), ..rect };
        Clear.render(rect, buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(Span::styled(
                " Skill Variables ",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(rect);
        block.render(rect, buf);

        let mut constraints: Vec<Constraint> = self.vars.iter().flat_map(|_| [
            Constraint::Length(1), // label
            Constraint::Length(1), // input
            Constraint::Length(1), // spacer
        ]).collect();
        constraints.push(Constraint::Min(1));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(inner);

        for (i, var) in self.vars.iter().enumerate() {
            let label_idx = i * 3;
            let input_idx = i * 3 + 1;
            if label_idx >= chunks.len() || input_idx >= chunks.len() {
                break;
            }
            let is_active = i == self.current;
            let label_style = if is_active {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Paragraph::new(Line::from(Span::styled(format!("{var}:"), label_style)))
                .render(chunks[label_idx], buf);
            let val = self.values.get(var).map(|s| s.as_str()).unwrap_or("");
            let input_style = if is_active {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let cursor = if is_active { "▏" } else { "" };
            Paragraph::new(Line::from(vec![
                Span::styled(val.to_string(), input_style),
                Span::raw(cursor),
            ]))
            .render(chunks[input_idx], buf);
        }
    }
}
