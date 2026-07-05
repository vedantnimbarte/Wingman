//! `/mode` picker — pick the session permission mode from a short list.
//!
//! Mirrors the `/model` picker but over a fixed, curated set of
//! [`wingman_config::PermissionMode`] values. Users who prefer typing can
//! still run `/mode <name>` directly.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};

use super::{centered_rect, ModalOutcome};

/// A selectable permission mode: the id emitted on Enter plus a blurb.
struct ModeEntry {
    /// Canonical id understood by `PermissionMode::from_str` / `/mode <id>`.
    id: &'static str,
    desc: &'static str,
}

/// Curated in ascending order of autonomy. `yolo` is intentionally omitted —
/// it is session-only, never persisted, and enabled via `--yolo`.
const MODES: &[ModeEntry] = &[
    ModeEntry {
        id: "read-only",
        desc: "Reads & searches run freely; every write or shell command asks first.",
    },
    ModeEntry {
        id: "plan",
        desc: "Read-only until you approve a plan, then auto-edits for the rest of the turn.",
    },
    ModeEntry {
        id: "auto-edit",
        desc: "Writes & shell inside the project run automatically; out-of-tree and risky commands still ask.",
    },
];

#[derive(Debug)]
pub struct ModePicker {
    selected: usize,
}

impl ModePicker {
    /// Open with `current` (the active mode id) pre-selected, falling back to
    /// the first entry when it isn't in the list.
    pub fn new(current: &str) -> Self {
        let selected = MODES.iter().position(|m| m.id == current).unwrap_or(0);
        Self { selected }
    }

    /// The highlighted mode id.
    pub fn take_selected(&self) -> Option<String> {
        MODES.get(self.selected).map(|m| m.id.to_string())
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        match k.code {
            KeyCode::Up if self.selected > 0 => self.selected -= 1,
            KeyCode::Down if self.selected + 1 < MODES.len() => self.selected += 1,
            KeyCode::Enter => return ModalOutcome::Close,
            _ => {}
        }
        ModalOutcome::Continue
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let rect = centered_rect(area, 60, 50);
        Clear.render(rect, buf);
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " /mode — select permission mode ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = outer.inner(rect);
        outer.render(rect, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(inner);

        let mut lines: Vec<Line> = Vec::new();
        for (i, m) in MODES.iter().enumerate() {
            let selected = i == self.selected;
            let marker = if selected { "› " } else { "  " };
            lines.push(Line::from(Span::styled(
                format!("{marker}{}", m.id),
                if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Cyan)
                },
            )));
            lines.push(Line::from(Span::styled(
                format!("    {}", m.desc),
                Style::default().fg(if selected {
                    Color::Gray
                } else {
                    Color::DarkGray
                }),
            )));
            lines.push(Line::from(""));
        }
        Paragraph::new(lines).render(chunks[0], buf);

        Paragraph::new(Line::from(Span::styled(
            "↑/↓ navigate · Enter select · Esc cancel",
            Style::default().fg(Color::DarkGray),
        )))
        .render(chunks[1], buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preselects_the_current_mode() {
        let p = ModePicker::new("auto-edit");
        assert_eq!(p.take_selected().as_deref(), Some("auto-edit"));
    }

    #[test]
    fn unknown_current_falls_back_to_first() {
        let p = ModePicker::new("nonsense");
        assert_eq!(p.take_selected().as_deref(), Some("read-only"));
    }

    #[test]
    fn arrows_move_and_clamp() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut p = ModePicker::new("read-only");
        // Up at the top is a no-op.
        p.handle_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(p.take_selected().as_deref(), Some("read-only"));
        p.handle_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(p.take_selected().as_deref(), Some("plan"));
        // Enter closes.
        assert!(matches!(
            p.handle_key(KeyEvent::from(KeyCode::Enter)),
            ModalOutcome::Close
        ));
    }
}
