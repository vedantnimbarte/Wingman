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
    /// When the agent is mid-turn, a "thinking…" indicator is appended below
    /// the last item so it lives in the message area rather than the input.
    pub busy: bool,
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

fn render_assistant_text(s: &str, code_color: Color) -> Vec<Line<'static>> {
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
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                // Closing fence — flush the buffered block, then the fence line.
                lines.extend(render_code_block(
                    std::mem::take(&mut code_buf),
                    current_lang.take().unwrap_or_default(),
                    code_color,
                ));
                in_code_block = false;
                lines.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::DarkGray),
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
            code_color,
        ));
    }
    lines
}

fn render_code_block(
    body_lines: Vec<String>,
    lang_label: String,
    fallback_color: Color,
) -> Vec<Line<'static>> {
    let body = body_lines.join("\n");
    #[cfg(feature = "treesitter")]
    {
        let lang = match lang_label.to_ascii_lowercase().as_str() {
            "rust" | "rs" => Some(arccode_ts::Language::Rust),
            "python" | "py" => Some(arccode_ts::Language::Python),
            "javascript" | "js" => Some(arccode_ts::Language::JavaScript),
            "typescript" | "ts" => Some(arccode_ts::Language::TypeScript),
            "tsx" => Some(arccode_ts::Language::Tsx),
            "go" => Some(arccode_ts::Language::Go),
            _ => None,
        };
        if let Some(lang) = lang {
            return highlight_body_to_lines(lang, &body, fallback_color);
        }
    }
    let _ = lang_label;
    plain_code_lines(&body, fallback_color)
}

fn plain_code_lines(body: &str, color: Color) -> Vec<Line<'static>> {
    body.lines()
        .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(color))))
        .collect()
}

#[cfg(feature = "treesitter")]
fn highlight_body_to_lines(
    lang: arccode_ts::Language,
    body: &str,
    fallback_color: Color,
) -> Vec<Line<'static>> {
    use arccode_ts::highlight::{highlight, HIGHLIGHT_NAMES};
    let spans = highlight(lang, body);
    // Build one Vec<Span> per source line, splitting spans on '\n'.
    let bytes = body.as_bytes();
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    for sp in spans {
        let start = sp.start_byte.min(bytes.len());
        let end = sp.end_byte.min(bytes.len());
        if start >= end {
            continue;
        }
        let style = match sp.scope.and_then(|i| HIGHLIGHT_NAMES.get(i).copied()) {
            Some(name) => scope_style(name, fallback_color),
            None => Style::default().fg(fallback_color),
        };
        let mut slice_start = start;
        while slice_start < end {
            let nl = bytes[slice_start..end].iter().position(|&b| b == b'\n');
            match nl {
                Some(rel) => {
                    let chunk_end = slice_start + rel;
                    if chunk_end > slice_start {
                        let text =
                            String::from_utf8_lossy(&bytes[slice_start..chunk_end]).into_owned();
                        lines.last_mut().unwrap().push(Span::styled(text, style));
                    }
                    lines.push(Vec::new());
                    slice_start = chunk_end + 1;
                }
                None => {
                    let text = String::from_utf8_lossy(&bytes[slice_start..end]).into_owned();
                    lines.last_mut().unwrap().push(Span::styled(text, style));
                    break;
                }
            }
        }
    }
    lines.into_iter().map(Line::from).collect()
}

#[cfg(feature = "treesitter")]
fn scope_style(name: &str, fallback: Color) -> Style {
    // Coarse mapping that works on dark and light themes. The fallback
    // ensures unmapped scopes still read as "code".
    let color = match name {
        "comment" => Color::DarkGray,
        "string" | "string.special" => Color::Green,
        "number" | "constant" | "constant.builtin" => Color::LightMagenta,
        "keyword" => Color::Magenta,
        "function" | "function.builtin" | "function.macro" => Color::Yellow,
        "type" | "type.builtin" => Color::Cyan,
        "variable.builtin" | "variable.parameter" => Color::LightBlue,
        "property" | "attribute" => Color::LightCyan,
        "label" | "tag" => Color::LightYellow,
        "operator" | "punctuation" | "punctuation.bracket" | "punctuation.delimiter" => Color::Gray,
        _ => fallback,
    };
    let mut s = Style::default().fg(color);
    if name == "keyword" {
        s = s.add_modifier(Modifier::BOLD);
    }
    s
}
