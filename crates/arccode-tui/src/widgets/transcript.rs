use crate::theme;
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
    /// Active search query (Ctrl+F). When `Some`, the render layer highlights
    /// matches and the view treats them as scroll anchors.
    pub search: Option<TranscriptSearch>,
}

#[derive(Debug, Clone, Default)]
pub struct TranscriptSearch {
    #[allow(dead_code)]
    pub query: String,
    /// Items containing the query (case-insensitive substring).
    pub hits: Vec<usize>,
    /// Index into `hits` for the current selection.
    pub cursor: usize,
}

impl Transcript {
    /// Begin or update an in-transcript search. Returns the number of hits.
    pub fn search_set(&mut self, query: &str) -> usize {
        let q = query.to_ascii_lowercase();
        if q.is_empty() {
            self.search = None;
            return 0;
        }
        let hits: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| item_text(it).to_ascii_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        let n = hits.len();
        self.search = Some(TranscriptSearch {
            query: query.to_string(),
            hits,
            cursor: 0,
        });
        n
    }
    pub fn search_next(&mut self) {
        if let Some(s) = self.search.as_mut() {
            if s.hits.is_empty() {
                return;
            }
            s.cursor = (s.cursor + 1) % s.hits.len();
        }
    }
    pub fn search_prev(&mut self) {
        if let Some(s) = self.search.as_mut() {
            if s.hits.is_empty() {
                return;
            }
            if s.cursor == 0 {
                s.cursor = s.hits.len() - 1;
            } else {
                s.cursor -= 1;
            }
        }
    }
    pub fn search_clear(&mut self) {
        self.search = None;
    }
}

fn item_text(it: &TranscriptItem) -> String {
    match it {
        TranscriptItem::UserPrompt(s) => s.clone(),
        TranscriptItem::AssistantText(s) => s.clone(),
        TranscriptItem::ToolCall { name, summary } => format!("{name} {summary}"),
        TranscriptItem::ToolResult { summary, .. } => summary.clone(),
        TranscriptItem::System(s) => s.clone(),
        TranscriptItem::Error(s) => s.clone(),
    }
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
        let th = theme::current();
        let mut lines: Vec<Line> = Vec::new();
        for item in &self.transcript.items {
            match item {
                TranscriptItem::UserPrompt(p) => {
                    lines.push(Line::from(vec![
                        Span::styled(
                            "› ",
                            Style::default()
                                .fg(th.user_prompt)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(p.clone()),
                    ]));
                    lines.push(Line::from(""));
                }
                TranscriptItem::AssistantText(s) => {
                    lines.extend(render_assistant_text(s, th.code_block));
                    lines.push(Line::from(""));
                }
                TranscriptItem::ToolCall { name, summary } => {
                    lines.push(Line::from(vec![
                        Span::styled("⚙ ", Style::default().fg(th.tool_name)),
                        Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)),
                        Span::raw(" "),
                        Span::styled(summary.clone(), Style::default().fg(th.tool_summary)),
                    ]));
                }
                TranscriptItem::ToolResult { ok, summary } => {
                    let glyph = if *ok { "✓" } else { "✗" };
                    let color = if *ok { th.tool_ok } else { th.tool_err };
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {glyph} "), Style::default().fg(color)),
                        Span::styled(summary.clone(), Style::default().fg(th.tool_summary)),
                    ]));
                    lines.push(Line::from(""));
                }
                TranscriptItem::System(s) => {
                    lines.push(Line::from(Span::styled(
                        s.clone(),
                        Style::default().fg(th.system),
                    )));
                }
                TranscriptItem::Error(s) => {
                    lines.push(Line::from(Span::styled(
                        format!("error: {s}"),
                        Style::default().fg(th.error),
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

fn render_assistant_text(s: &str, code_color: Color) -> Vec<Line<'static>> {
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
                Style::default().fg(code_color),
            )));
        } else {
            lines.push(Line::from(Span::raw(line.to_string())));
        }
    }
    lines
}
