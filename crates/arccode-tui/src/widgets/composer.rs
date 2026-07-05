use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};

#[derive(Debug, Default)]
pub struct Composer {
    pub input: String,
    pub busy: bool,
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
}

impl Composer {
    pub fn clear(&mut self) {
        self.input.clear();
        self.history_idx = None;
    }

    pub fn take_input(&mut self) -> String {
        let s = std::mem::take(&mut self.input);
        if !s.trim().is_empty() {
            self.history.push(s.clone());
        }
        self.history_idx = None;
        s
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_idx {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_idx = Some(next);
        self.input = self.history[next].clone();
    }

    pub fn history_next(&mut self) {
        match self.history_idx {
            None => {}
            Some(i) if i + 1 >= self.history.len() => {
                self.history_idx = None;
                self.input.clear();
            }
            Some(i) => {
                self.history_idx = Some(i + 1);
                self.input = self.history[i + 1].clone();
            }
        }
    }
}

pub struct ComposerView<'a> {
    pub composer: &'a Composer,
}

impl<'a> Widget for ComposerView<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (title, title_style, border_style) = if self.composer.busy {
            (
                " ⏳ working ",
                Style::default().fg(Color::Yellow),
                Style::default().fg(Color::Yellow),
            )
        } else {
            (
                " › ",
                Style::default().fg(Color::Cyan),
                Style::default().fg(Color::Cyan),
            )
        };
        // Tailor the placeholder to the available horizontal space so it
        // never gets cropped or visually overruns the cursor.
        // Subtract 2 for left/right borders when estimating inner width.
        let inner_width = area.width.saturating_sub(2);
        let hint = if inner_width >= 60 {
            " type a message · / for commands · @ to attach a file"
        } else if inner_width >= 40 {
            " type a message · / commands · @ files"
        } else {
            " type / for commands"
        };

        let line = if self.composer.busy {
            // The thinking indicator now lives in the message area; while busy
            // the composer just shows a muted "working" hint (or any text the
            // user had already typed) so the input isn't hijacked.
            if self.composer.input.is_empty() {
                Line::from(Span::styled(
                    " working…",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ))
            } else {
                Line::from(Span::styled(
                    self.composer.input.clone(),
                    Style::default().fg(Color::DarkGray),
                ))
            }
        } else if self.composer.input.is_empty() {
            Line::from(vec![
                Span::styled("▏", Style::default().fg(Color::Cyan)),
                Span::styled(
                    hint,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ),
            ])
        } else {
            Line::from(vec![
                Span::raw(self.composer.input.clone()),
                Span::styled("▏", Style::default().fg(Color::Cyan)),
            ])
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(Span::styled(title, title_style));

        // Use Paragraph::block() so ratatui owns the layout and places the
        // text inside the border rect — avoids any off-by-one with manual
        // block.inner() + separate renders.
        Paragraph::new(line).block(block).render(area, buf);
    }
}
