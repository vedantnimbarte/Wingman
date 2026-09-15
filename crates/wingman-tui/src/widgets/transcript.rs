use crate::theme::{self, Theme};
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
    /// Model reasoning. Rendered in full while it is the newest item (it is
    /// the live "what is it doing"), then collapsed to one line once the
    /// answer starts — reasoning is usually far longer than the answer and
    /// would otherwise bury it.
    Thinking(String),
    ToolCall {
        name: String,
        summary: String,
    },
    ToolResult {
        ok: bool,
        summary: String,
    },
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
        TranscriptItem::Thinking(s) => s.clone(),
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

    pub fn append_thinking(&mut self, text: &str) {
        if let Some(TranscriptItem::Thinking(s)) = self.items.last_mut() {
            s.push_str(text);
        } else {
            self.items.push(TranscriptItem::Thinking(text.into()));
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
    /// When the agent is mid-turn, a "thinking…" indicator is appended below
    /// the last item so it lives in the message area rather than the input.
    pub busy: bool,
}

impl<'a> Widget for TranscriptView<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let th = theme::current();
        let mut lines: Vec<Line> = Vec::new();
        let last_idx = self.transcript.items.len().saturating_sub(1);
        for (idx, item) in self.transcript.items.iter().enumerate() {
            let is_last = idx == last_idx;
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
                    lines.extend(render_assistant_text(s, &th));
                    lines.push(Line::from(""));
                }
                TranscriptItem::Thinking(s) => {
                    lines.extend(render_thinking(s, is_last));
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
        // While the agent is working, show the thinking indicator inline at
        // the bottom of the message area (not in the input composer).
        if self.busy {
            lines.push(Line::from(Span::styled(
                "⏳ thinking…",
                Style::default()
                    .fg(th.system)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
        let block = Block::default().borders(Borders::NONE);
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((self.transcript.scroll, 0))
            .render(area, buf);
    }
}

/// Dim, italic, and marked `✻`. Expanded only while it is the newest item;
/// after that just a header with the line count, so a long chain of thought
/// does not push the answer off screen.
fn render_thinking(s: &str, expanded: bool) -> Vec<Line<'static>> {
    let style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);
    let body: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    let header = if expanded || body.len() <= 1 {
        "✻ thinking".to_string()
    } else {
        format!("✻ thinking ({} lines)", body.len())
    };
    let mut lines = vec![Line::from(Span::styled(header, style))];
    if expanded {
        lines.extend(
            body.into_iter()
                .map(|l| Line::from(Span::styled(format!("  {l}"), style))),
        );
    } else if let Some(first) = body.first() {
        lines.push(Line::from(Span::styled(
            format!("  {}", truncate_chars(first, 100)),
            style,
        )));
    }
    lines
}

/// Cut to `max` characters on a char boundary, appending an ellipsis.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

fn render_assistant_text(s: &str, th: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code_block = false;
    let mut current_lang: Option<String> = None;
    let mut code_buf: Vec<String> = Vec::new();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("```") {
            if !in_code_block {
                in_code_block = true;
                current_lang = Some(rest.trim().to_string());
                lines.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(th.system),
                )));
            } else {
                // Closing fence — flush the buffered block, then the fence line.
                lines.extend(render_code_block(
                    std::mem::take(&mut code_buf),
                    current_lang.take().unwrap_or_default(),
                    th,
                ));
                in_code_block = false;
                lines.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(th.system),
                )));
            }
            continue;
        }
        if in_code_block {
            code_buf.push(line.to_string());
        } else {
            lines.push(Line::from(Span::raw(line.to_string())));
        }
    }
    // Unclosed code fence at EOF — render what we have.
    if in_code_block && !code_buf.is_empty() {
        lines.extend(render_code_block(
            code_buf,
            current_lang.unwrap_or_default(),
            th,
        ));
    }
    lines
}

fn render_code_block(
    body_lines: Vec<String>,
    lang_label: String,
    th: &Theme,
) -> Vec<Line<'static>> {
    super::code::fence_lines(&body_lines.join("\n"), &lang_label, th)
}
