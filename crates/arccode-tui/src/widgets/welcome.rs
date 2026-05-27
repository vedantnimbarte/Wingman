//! Empty-state welcome screen, shown when there is no conversation history.
//!
//! Mirrors the style of Claude Code's splash screen: centred logo, active
//! model/provider, permission mode, and a compact shortcut reference.

use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};

use crate::widgets::StatusLine;

const LOGO: &[&str] = &[
    r"     _             ____          _      ",
    r"    / \   _ __ ___|  _ \ ___  __| | ___ ",
    r"   / _ \ | '__/ __| |_) / _ \/ _` |/ _ \",
    r"  / ___ \| | | (__|  _ <  __/ (_| |  __/",
    r" /_/   \_\_|  \___|_| \_\___|\__,_|\___|",
];

pub struct WelcomeView<'a> {
    pub status: &'a StatusLine,
}

impl<'a> Widget for WelcomeView<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Card dimensions: logo(5) + blank(1) + tagline(1) + blank(1) +
        // info rows(3) + blank(1) + shortcut header(1) + shortcuts(5) + blank(1) = 19 rows
        // + 2 border rows = 21 total
        let card_h = 21u16;
        let card_w = 60u16;

        if area.height < card_h || area.width < card_w {
            // Fallback: just print a compact single-line welcome.
            let s = self.status;
            let line = if s.provider.is_empty() {
                "ArcCode — no provider configured. Type /help to get started.".to_string()
            } else {
                format!(
                    "ArcCode · {}/{} · mode={} · /help for commands",
                    s.provider, s.model, s.mode
                )
            };
            Paragraph::new(line)
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::DarkGray))
                .render(area, buf);
            return;
        }

        // Centre the card.
        let x = area.x + area.width.saturating_sub(card_w) / 2;
        let y = area.y + area.height.saturating_sub(card_h) / 2;
        let card = Rect { x, y, width: card_w, height: card_h };

        Clear.render(card, buf);
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = outer.inner(card);
        outer.render(card, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5), // ASCII logo
                Constraint::Length(1), // blank
                Constraint::Length(1), // tagline
                Constraint::Length(1), // blank
                Constraint::Length(1), // provider / model row
                Constraint::Length(1), // mode row
                Constraint::Length(1), // version / tools row
                Constraint::Length(1), // blank
                Constraint::Length(1), // shortcut header
                Constraint::Length(1), // shortcut row 1
                Constraint::Length(1), // shortcut row 2
                Constraint::Length(1), // shortcut row 3
                Constraint::Length(1), // shortcut row 4
                Constraint::Min(0),    // padding
            ])
            .split(inner);

        // ── Logo ──────────────────────────────────────────────────────────
        for (i, line) in LOGO.iter().enumerate() {
            if i >= chunks[0].height as usize {
                break;
            }
            let row = Rect { y: chunks[0].y + i as u16, ..chunks[0] };
            Paragraph::new(Line::from(Span::styled(
                *line,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )))
            .alignment(Alignment::Center)
            .render(row, buf);
        }

        // ── Tagline ───────────────────────────────────────────────────────
        Paragraph::new(Line::from(Span::styled(
            "multi-provider terminal coding agent",
            Style::default().fg(Color::DarkGray),
        )))
        .alignment(Alignment::Center)
        .render(chunks[2], buf);

        // ── Info rows ─────────────────────────────────────────────────────
        let s = self.status;

        let (provider_label, model_label) = if s.provider.is_empty() {
            (
                Span::styled("no provider", Style::default().fg(Color::Red)),
                Span::styled(
                    "  run /login to configure",
                    Style::default().fg(Color::DarkGray),
                ),
            )
        } else {
            (
                Span::styled(
                    s.provider.clone(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}", s.model),
                    Style::default().fg(Color::White),
                ),
            )
        };

        Paragraph::new(Line::from(vec![
            Span::styled("  model  ", Style::default().fg(Color::DarkGray)),
            provider_label,
            Span::styled("/", Style::default().fg(Color::DarkGray)),
            model_label,
        ]))
        .render(chunks[4], buf);

        let mode_color = match s.mode.as_str() {
            "yolo" => Color::Red,
            "auto-edit" => Color::Yellow,
            _ => Color::Green,
        };
        Paragraph::new(Line::from(vec![
            Span::styled("  mode   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                s.mode.clone(),
                Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
            ),
        ]))
        .render(chunks[5], buf);

        Paragraph::new(Line::from(vec![
            Span::styled("  tools  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "read_file · write_file · edit_file · run_shell · glob · grep",
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .render(chunks[6], buf);

        // ── Shortcut hints ────────────────────────────────────────────────
        Paragraph::new(Line::from(Span::styled(
            "  shortcuts",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::UNDERLINED),
        )))
        .render(chunks[8], buf);

        let shortcuts: &[(&str, &str)] = &[
            ("  Enter", "send a message"),
            ("  @", "attach a file from the project"),
            ("  /model", "switch provider or model"),
            ("  /help", "full command reference  ·  Ctrl-C exit"),
        ];
        for (i, (key, desc)) in shortcuts.iter().enumerate() {
            let row_idx = 9 + i;
            if row_idx >= chunks.len() {
                break;
            }
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{key:<10}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(*desc, Style::default().fg(Color::DarkGray)),
            ]))
            .render(chunks[row_idx], buf);
        }
    }
}
