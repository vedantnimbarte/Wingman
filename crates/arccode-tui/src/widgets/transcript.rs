use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget, Wrap},
};

#[derive(Debug, Clone)]
pub enum TranscriptItem {
    UserPrompt(String),
    AssistantText(String),
    ToolCall { name: String, summary: String },
    ToolResult { ok: bool, summary: String },
    System(String),
    Error(String),
}

#[derive(Debug, Default)]
pub struct Transcript {
    pub items: Vec<TranscriptItem>,
    pub scroll: u16,
}

impl Transcript {
    pub fn push(&mut self, item: TranscriptItem) {
        self.items.push(item);
    }

    /// Append text to the last assistant item, or start a new one.
    pub fn append_assistant_text(&mut self, text: &str) {
        if let Some(TranscriptItem::AssistantText(s)) = self.items.last_mut() {
            s.push_str(text);
        } else {
            self.items.push(TranscriptItem::AssistantText(text.into()));
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.scroll = 0;
    }

    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_add(1);
    }
}

pub struct TranscriptView<'a> {
    pub transcript: &'a Transcript,
}

impl<'a> Widget for TranscriptView<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut lines: Vec<Line> = Vec::new();
        for item in &self.transcript.items {
            match item {
                TranscriptItem::UserPrompt(p) => {
                    lines.push(Line::from(vec![
                        Span::styled(
                            "› ",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(p.clone()),
                    ]));
                    lines.push(Line::from(""));
                }
                TranscriptItem::AssistantText(s) => {
                    lines.extend(render_assistant_text(s));
                    lines.push(Line::from(""));
                }
                TranscriptItem::ToolCall { name, summary } => {
                    lines.push(Line::from(vec![
                        Span::styled("⚙ ", Style::default().fg(Color::Yellow)),
                        Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)),
                        Span::raw(" "),
                        Span::styled(summary.clone(), Style::default().fg(Color::DarkGray)),
                    ]));
                }
                TranscriptItem::ToolResult { ok, summary } => {
                    let glyph = if *ok { "✓" } else { "✗" };
                    let color = if *ok { Color::Green } else { Color::Red };
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {glyph} "), Style::default().fg(color)),
                        Span::styled(summary.clone(), Style::default().fg(Color::DarkGray)),
                    ]));
                    lines.push(Line::from(""));
                }
                TranscriptItem::System(s) => {
                    lines.push(Line::from(Span::styled(
                        s.clone(),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                TranscriptItem::Error(s) => {
                    lines.push(Line::from(Span::styled(
                        format!("error: {s}"),
                        Style::default().fg(Color::Red),
                    )));
                }
            }
        }
        let block = Block::default().borders(Borders::NONE);
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((self.transcript.scroll, 0))
            .render(area, buf);
    }
}

fn render_assistant_text(s: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code_block = false;
    for line in s.lines() {
        if line.starts_with("```") {
            in_code_block = !in_code_block;
            lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::DarkGray),
            )));
        } else if in_code_block {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::Yellow),
            )));
        } else {
            lines.push(Line::from(Span::raw(line.to_string())));
        }
    }
    lines
}
