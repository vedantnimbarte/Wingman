//! Keyboard shortcuts help overlay.

use crossterm::event::KeyEvent;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap},
};

use super::{centered_rect, ModalOutcome};

#[derive(Debug, Default)]
pub struct HelpModal;

impl HelpModal {
    pub fn new() -> Self {
        Self
    }

    pub fn handle_key(&mut self, _k: KeyEvent) -> ModalOutcome {
        // Any key closes the help overlay.
        ModalOutcome::Close
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let rect = centered_rect(area, 80, 80);
        Clear.render(rect, buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " Keyboard Shortcuts — press any key to close ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(rect);
        block.render(rect, buf);

        let lines = vec![
            Line::from(Span::styled(
                "Navigation",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled("  Enter      ", Style::default().fg(Color::Cyan)),
                Span::raw("Submit prompt"),
            ]),
            Line::from(vec![
                Span::styled("  Esc        ", Style::default().fg(Color::Cyan)),
                Span::raw("Clear input"),
            ]),
            Line::from(vec![
                Span::styled("  Ctrl+C     ", Style::default().fg(Color::Cyan)),
                Span::raw("Exit"),
            ]),
            Line::from(vec![
                Span::styled("  Up/Down    ", Style::default().fg(Color::Cyan)),
                Span::raw("History / autocomplete navigation"),
            ]),
            Line::from(vec![
                Span::styled("  Tab        ", Style::default().fg(Color::Cyan)),
                Span::raw("Complete slash command"),
            ]),
            Line::from(vec![
                Span::styled("  PgUp/PgDn  ", Style::default().fg(Color::Cyan)),
                Span::raw("Scroll transcript"),
            ]),
            Line::from(vec![
                Span::styled("  @          ", Style::default().fg(Color::Cyan)),
                Span::raw("Open fuzzy file picker"),
            ]),
            Line::from(vec![
                Span::styled("  ?          ", Style::default().fg(Color::Cyan)),
                Span::raw("Show this help"),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Slash Commands",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled("  /help          ", Style::default().fg(Color::Cyan)),
                Span::raw("Show command reference"),
            ]),
            Line::from(vec![
                Span::styled("  /clear         ", Style::default().fg(Color::Cyan)),
                Span::raw("Reset conversation"),
            ]),
            Line::from(vec![
                Span::styled("  /login         ", Style::default().fg(Color::Cyan)),
                Span::raw("Set up a provider (guided wizard)"),
            ]),
            Line::from(vec![
                Span::styled("  /logout [p]    ", Style::default().fg(Color::Cyan)),
                Span::raw("Remove stored API key"),
            ]),
            Line::from(vec![
                Span::styled("  /model [p/m]   ", Style::default().fg(Color::Cyan)),
                Span::raw("Switch model or open picker"),
            ]),
            Line::from(vec![
                Span::styled("  /params        ", Style::default().fg(Color::Cyan)),
                Span::raw("Adjust temperature / max_tokens"),
            ]),
            Line::from(vec![
                Span::styled("  /mode [m]      ", Style::default().fg(Color::Cyan)),
                Span::raw("Switch permission mode, or open a picker with no arg"),
            ]),
            Line::from(vec![
                Span::styled("  /add <path>    ", Style::default().fg(Color::Cyan)),
                Span::raw("Attach file to next prompt"),
            ]),
            Line::from(vec![
                Span::styled("  /export [md|json]", Style::default().fg(Color::Cyan)),
                Span::raw("Export conversation"),
            ]),
            Line::from(vec![
                Span::styled("  /resume        ", Style::default().fg(Color::Cyan)),
                Span::raw("Resume a previous session"),
            ]),
            Line::from(vec![
                Span::styled("  /usage         ", Style::default().fg(Color::Cyan)),
                Span::raw("Token + cost breakdown"),
            ]),
            Line::from(vec![
                Span::styled("  /skills        ", Style::default().fg(Color::Cyan)),
                Span::raw("Browse skills"),
            ]),
            Line::from(vec![
                Span::styled("  /skill <name>  ", Style::default().fg(Color::Cyan)),
                Span::raw("Queue a skill for next prompt"),
            ]),
            Line::from(vec![
                Span::styled("  /mcp           ", Style::default().fg(Color::Cyan)),
                Span::raw("Manage MCP servers"),
            ]),
            Line::from(vec![
                Span::styled("  /quit          ", Style::default().fg(Color::Cyan)),
                Span::raw("Exit"),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "File Picker",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled("  Space      ", Style::default().fg(Color::Cyan)),
                Span::raw("Toggle file selection"),
            ]),
            Line::from(vec![
                Span::styled("  Enter      ", Style::default().fg(Color::Cyan)),
                Span::raw("Attach selected file(s)"),
            ]),
        ];

        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(inner, buf);
    }
}
