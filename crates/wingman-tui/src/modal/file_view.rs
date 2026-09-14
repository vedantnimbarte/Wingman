//! Read-only view of one file, opened with `v` from the file-tree sidebar.
//! Code is syntax highlighted by extension through the theme.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};

use super::{centered_rect, ModalOutcome};
use crate::theme::Theme;
use crate::widgets::code;

/// Bytes shown from the head of a file. Highlighting is done once, up
/// front, so this bounds the work as well as the memory.
const MAX_VIEW_BYTES: usize = 512 * 1024;

#[derive(Debug)]
pub struct FileViewModal {
    title: String,
    lines: Vec<Line<'static>>,
    scroll: usize,
}

impl FileViewModal {
    /// `title` is what the frame shows (the project-relative path); `path`
    /// is read and decides the language.
    pub fn new(title: String, path: &Path, th: &Theme) -> Self {
        let lines = match std::fs::read(path) {
            Ok(bytes) => view_lines(&bytes, path, th),
            Err(e) => vec![note(format!("cannot read: {e}"), th)],
        };
        Self {
            title,
            lines,
            scroll: 0,
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        let last = self.lines.len().saturating_sub(1);
        self.scroll = match k.code {
            KeyCode::Char('q') => return ModalOutcome::Close,
            KeyCode::Up | KeyCode::Char('k') => self.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll.saturating_add(1),
            KeyCode::PageUp => self.scroll.saturating_sub(20),
            KeyCode::PageDown => self.scroll.saturating_add(20),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            _ => self.scroll,
        }
        .min(last);
        ModalOutcome::Continue
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let rect = centered_rect(area, 90, 90);
        Clear.render(rect, buf);
        let block = Block::default().borders(Borders::ALL).title(Span::styled(
            format!(" {} — j/k PgUp/PgDn g/G · q close ", self.title),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        // Not wrapped: a wrapped line of code reads as two lines, and the
        // gutter numbers would stop matching the rows beside them. Only the
        // rows on screen are handed over, not the whole file every frame.
        let visible = self.lines[self.scroll.min(self.lines.len())..]
            .iter()
            .take(rect.height as usize)
            .cloned()
            .collect::<Vec<_>>();
        Paragraph::new(visible).block(block).render(rect, buf);
    }
}

fn view_lines(bytes: &[u8], path: &Path, th: &Theme) -> Vec<Line<'static>> {
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return vec![note("binary file — not shown".into(), th)];
    }
    let head = &bytes[..bytes.len().min(MAX_VIEW_BYTES)];
    // A cut through a multi-byte character is the only invalid UTF-8 the
    // cap can introduce; anything earlier is a file that is not text.
    let text = match std::str::from_utf8(head) {
        Ok(t) => t,
        Err(e) if e.error_len().is_none() => {
            std::str::from_utf8(&head[..e.valid_up_to()]).unwrap_or_default()
        }
        Err(_) => return vec![note("not UTF-8 text — not shown".into(), th)],
    };
    let body = code::terminal_safe(text);
    let mut lines = code::file_lines(&body, path, th);
    let width = lines.len().max(1).to_string().len();
    for (i, line) in lines.iter_mut().enumerate() {
        line.spans.insert(
            0,
            Span::styled(
                format!("{:>width$} ", i + 1),
                Style::default().fg(th.system),
            ),
        );
    }
    if bytes.len() > head.len() {
        lines.push(note(
            format!(
                "… first {} KiB of {} KiB shown",
                text.len() / 1024,
                bytes.len() / 1024
            ),
            th,
        ));
    }
    lines
}

fn note(text: String, th: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        text,
        Style::default()
            .fg(th.system)
            .add_modifier(Modifier::ITALIC),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn numbers_lines_and_bounds_what_it_reads() {
        let th = crate::theme::resolve(&Default::default(), false);
        let dir = tempfile::tempdir().unwrap();

        let src = dir.path().join("main.rs");
        std::fs::write(&src, "fn main() {\n\tprintln!(\"\x1b[2J\");\n}\n").unwrap();
        let mut view = FileViewModal::new("main.rs".into(), &src, &th);
        let rows: Vec<_> = view.lines.iter().map(text).collect();
        // Tabs expand and escape bytes cannot reach the terminal.
        assert_eq!(
            rows,
            ["1 fn main() {", "2     println!(\"\u{fffd}[2J\");", "3 }"]
        );
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        view.handle_key(key(KeyCode::End));
        assert_eq!(view.scroll, 2);
        view.handle_key(key(KeyCode::PageDown));
        assert_eq!(view.scroll, 2);
        assert!(matches!(
            view.handle_key(key(KeyCode::Char('q'))),
            ModalOutcome::Close
        ));

        let big = dir.path().join("big.txt");
        // The leading byte puts the cap through the middle of an é.
        std::fs::write(&big, format!("a{}", "é".repeat(MAX_VIEW_BYTES))).unwrap();
        let view = FileViewModal::new("big.txt".into(), &big, &th);
        assert_eq!(view.lines.len(), 2);
        assert!(text(&view.lines[1]).starts_with("… first 511 KiB of 1024 KiB"));

        let bin = dir.path().join("a.bin");
        std::fs::write(&bin, b"\x7fELF\0\0").unwrap();
        let view = FileViewModal::new("a.bin".into(), &bin, &th);
        assert_eq!(text(&view.lines[0]), "binary file — not shown");

        let missing = FileViewModal::new("gone".into(), &dir.path().join("gone"), &th);
        assert!(text(&missing.lines[0]).starts_with("cannot read:"));
    }
}
