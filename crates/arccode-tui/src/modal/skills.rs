//! `/skills` list modal.
//!
//! Browses skills loaded by [`arccode_skills::load_all`]. Enter on a row
//! emits a [`ModalOutcome::Close`]; the host extracts the selected skill
//! via [`SkillsView::take_selected`] and prepends its body to the next
//! prompt.

use arccode_skills::Skill;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Widget},
};

use super::{centered_rect, ModalOutcome};

const VISIBLE_ROWS: usize = 12;

#[derive(Debug)]
pub struct SkillsView {
    skills: Vec<Skill>,
    selected: usize,
}

impl SkillsView {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self {
            skills,
            selected: 0,
        }
    }

    pub fn take_selected(&mut self) -> Option<Skill> {
        self.skills.get(self.selected).cloned()
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        match k.code {
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            KeyCode::Down => {
                if self.selected + 1 < self.skills.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Enter => {
                if self.skills.is_empty() {
                    return ModalOutcome::Continue;
                }
                return ModalOutcome::Close;
            }
            _ => {}
        }
        ModalOutcome::Continue
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let rect = centered_rect(area, 78, 70);
        Clear.render(rect, buf);
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                format!(" /skills — {} available ", self.skills.len()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = outer.inner(rect);
        outer.render(rect, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),    // list
                Constraint::Length(4), // preview
                Constraint::Length(1), // hint
            ])
            .split(inner);

        if self.skills.is_empty() {
            Paragraph::new(Line::from(Span::styled(
                "(no skills yet — try `/skills new <name>`)",
                Style::default().fg(Color::DarkGray),
            )))
            .render(chunks[0], buf);
        } else {
            let height = chunks[0].height as usize;
            let visible = height.max(1).min(VISIBLE_ROWS);
            let start = self.selected.saturating_sub(visible.saturating_sub(1));
            let end = (start + visible).min(self.skills.len());
            let items: Vec<ListItem> = self.skills[start..end]
                .iter()
                .enumerate()
                .map(|(off, s)| {
                    let i = start + off;
                    let marker = if i == self.selected { "› " } else { "  " };
                    let style = if i == self.selected {
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let line = Line::from(vec![
                        Span::styled(format!("{marker}{:<24}", s.name), style),
                        Span::styled(
                            s.description.clone(),
                            Style::default().fg(Color::Gray),
                        ),
                        Span::raw("  "),
                        Span::styled(
                            format!("[{}]", s.source.label()),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]);
                    ListItem::new(line)
                })
                .collect();
            List::new(items).render(chunks[0], buf);
        }

        // Preview pane: first ~3 lines of the selected skill's body.
        let preview = self
            .skills
            .get(self.selected)
            .map(|s| {
                s.body
                    .lines()
                    .take(3)
                    .map(|l| {
                        if l.len() > 100 {
                            format!("{}…", &l[..100])
                        } else {
                            l.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let preview_block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(" preview ", Style::default().fg(Color::DarkGray)));
        Paragraph::new(preview)
            .block(preview_block)
            .style(Style::default().fg(Color::Gray))
            .render(chunks[1], buf);

        Paragraph::new(Line::from(Span::styled(
            "↑/↓ navigate · Enter use · Esc cancel",
            Style::default().fg(Color::DarkGray),
        )))
        .render(chunks[2], buf);
    }
}
