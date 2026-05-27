//! `/usage` modal — per-model token + cost breakdown.
//!
//! Two tabs:
//!   - **Session**: usage rows accumulated since this TUI instance started.
//!   - **Lifetime**: session merged with `~/.arccode/usage.json`.
//!
//! Cost is best-effort: models not in [`arccode_core::pricing`] render `—`.

use std::collections::BTreeMap;

use arccode_core::{price_for, Usage};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table, Widget},
};

use super::{centered_rect, ModalOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Session,
    Lifetime,
}

#[derive(Debug)]
pub struct UsageView {
    session: BTreeMap<String, Usage>,
    lifetime: BTreeMap<String, Usage>,
    tab: Tab,
}

impl UsageView {
    pub fn new(session: BTreeMap<String, Usage>, lifetime: BTreeMap<String, Usage>) -> Self {
        Self {
            session,
            lifetime,
            tab: Tab::Session,
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        match k.code {
            KeyCode::Tab | KeyCode::Right | KeyCode::Left => {
                self.tab = match self.tab {
                    Tab::Session => Tab::Lifetime,
                    Tab::Lifetime => Tab::Session,
                };
            }
            KeyCode::Char('s') | KeyCode::Char('S') => self.tab = Tab::Session,
            KeyCode::Char('l') | KeyCode::Char('L') => self.tab = Tab::Lifetime,
            _ => {}
        }
        ModalOutcome::Continue
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let rect = centered_rect(area, 80, 70);
        Clear.render(rect, buf);
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " /usage — token + cost breakdown ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = outer.inner(rect);
        outer.render(rect, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // tab strip
                Constraint::Min(3),    // table
                Constraint::Length(1), // hint
            ])
            .split(inner);

        self.render_tabs(chunks[0], buf);
        self.render_table(chunks[1], buf);
        Paragraph::new(Line::from(Span::styled(
            "Tab/←→ switch tabs · S/L jump · Esc close",
            Style::default().fg(Color::DarkGray),
        )))
        .render(chunks[2], buf);
    }

    fn render_tabs(&self, area: Rect, buf: &mut Buffer) {
        let make = |label: &str, active: bool| {
            if active {
                Span::styled(
                    format!(" {label} "),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    format!(" {label} "),
                    Style::default().fg(Color::DarkGray),
                )
            }
        };
        let line = Line::from(vec![
            make("Session", self.tab == Tab::Session),
            Span::raw("  "),
            make("Lifetime", self.tab == Tab::Lifetime),
        ]);
        Paragraph::new(line).render(area, buf);
    }

    fn render_table(&self, area: Rect, buf: &mut Buffer) {
        let data = match self.tab {
            Tab::Session => &self.session,
            Tab::Lifetime => &self.lifetime,
        };
        if data.is_empty() {
            let msg = match self.tab {
                Tab::Session => "(no usage this session yet)",
                Tab::Lifetime => "(no recorded usage)",
            };
            Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(Color::DarkGray),
            )))
            .render(area, buf);
            return;
        }

        let header = Row::new(vec![
            "model",
            "input",
            "output",
            "cache rd",
            "cache wr",
            "cost",
        ])
        .style(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::UNDERLINED),
        );

        let mut rows: Vec<Row> = data
            .iter()
            .map(|(model, u)| {
                let cost = price_for(model)
                    .map(|p| format!("${:.4}", p.cost(u)))
                    .unwrap_or_else(|| "—".into());
                Row::new(vec![
                    model.clone(),
                    fmt(u.input_tokens),
                    fmt(u.output_tokens),
                    fmt(u.cache_read_input_tokens),
                    fmt(u.cache_creation_input_tokens),
                    cost,
                ])
            })
            .collect();

        // Totals row.
        let total = sum(data);
        let total_cost: Option<f64> = {
            let mut sum = 0.0;
            let mut any = false;
            for (model, u) in data {
                if let Some(p) = price_for(model) {
                    sum += p.cost(u);
                    any = true;
                }
            }
            any.then_some(sum)
        };
        rows.push(
            Row::new(vec![
                "TOTAL".to_string(),
                fmt(total.input_tokens),
                fmt(total.output_tokens),
                fmt(total.cache_read_input_tokens),
                fmt(total.cache_creation_input_tokens),
                total_cost
                    .map(|c| format!("${c:.4}"))
                    .unwrap_or_else(|| "—".into()),
            ])
            .style(Style::default().add_modifier(Modifier::BOLD)),
        );

        let widths = [
            Constraint::Percentage(38),
            Constraint::Percentage(12),
            Constraint::Percentage(12),
            Constraint::Percentage(12),
            Constraint::Percentage(12),
            Constraint::Percentage(14),
        ];
        Table::new(rows, widths).header(header).render(area, buf);
    }
}

fn fmt(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn sum(data: &BTreeMap<String, Usage>) -> Usage {
    let mut total = Usage::default();
    for u in data.values() {
        total.add(u);
    }
    total
}
