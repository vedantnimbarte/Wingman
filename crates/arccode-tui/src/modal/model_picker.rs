//! `/model` picker — flat fuzzy list of `provider/model` entries.
//!
//! For each provider known to the login wizard ([`super::login::PROVIDERS`])
//! we expose a small curated set of well-known models. The list is
//! deliberately short; users who want an obscure model can still type
//! `/model provider/<model-id>` directly.

use crossterm::event::{KeyCode, KeyEvent};
use nucleo_matcher::{
    pattern::{CaseMatching, Normalization, Pattern},
    Config, Matcher, Utf32Str,
};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Widget},
};

use super::{centered_rect, ModalOutcome};

const VISIBLE_ROWS: usize = 12;

/// (provider_id, model_id) — what the picker emits on Enter.
#[derive(Debug, Clone)]
pub struct ModelChoice {
    pub provider_id: String,
    pub model: String,
}

#[derive(Debug)]
pub struct ModelPicker {
    /// All known `provider/model` entries.
    entries: Vec<ModelChoice>,
    /// Indices into `entries` ranked by the current query.
    ranked: Vec<usize>,
    query: String,
    selected: usize,
}

impl ModelPicker {
    pub fn new() -> Self {
        let entries = catalog();
        let ranked: Vec<usize> = (0..entries.len()).collect();
        Self {
            entries,
            ranked,
            query: String::new(),
            selected: 0,
        }
    }

    /// Returns the currently-highlighted choice, if any.
    pub fn take_selected(&mut self) -> Option<ModelChoice> {
        self.ranked
            .get(self.selected)
            .and_then(|&i| self.entries.get(i))
            .cloned()
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        match k.code {
            KeyCode::Char(c) => {
                self.query.push(c);
                self.rerank();
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.rerank();
            }
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            KeyCode::Down => {
                if self.selected + 1 < self.ranked.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Enter => {
                if self.ranked.is_empty() {
                    return ModalOutcome::Continue;
                }
                return ModalOutcome::Close;
            }
            _ => {}
        }
        ModalOutcome::Continue
    }

    fn rerank(&mut self) {
        self.selected = 0;
        if self.query.is_empty() {
            self.ranked = (0..self.entries.len()).collect();
            return;
        }
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(usize, u32)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                buf.clear();
                let label = format!("{}/{}", e.provider_id, e.model);
                let needle = Utf32Str::new(&label, &mut buf);
                pattern.score(needle, &mut matcher).map(|s| (i, s))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        self.ranked = scored.into_iter().map(|(i, _)| i).collect();
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let rect = centered_rect(area, 60, 60);
        Clear.render(rect, buf);
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " /model — pick provider + model ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = outer.inner(rect);
        outer.render(rect, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // filter
                Constraint::Min(3),    // list
                Constraint::Length(2), // hint
            ])
            .split(inner);

        Paragraph::new(Line::from(vec![
            Span::styled("filter: ", Style::default().fg(Color::DarkGray)),
            Span::styled(self.query.clone(), Style::default().fg(Color::White)),
            Span::raw("▏"),
        ]))
        .render(chunks[0], buf);

        let height = chunks[1].height as usize;
        let visible = height.max(1).min(VISIBLE_ROWS);
        let start = self.selected.saturating_sub(visible.saturating_sub(1));
        let end = (start + visible).min(self.ranked.len());
        let items: Vec<ListItem> = self.ranked[start..end]
            .iter()
            .enumerate()
            .map(|(off, &idx)| {
                let i = start + off;
                let entry = &self.entries[idx];
                let marker = if i == self.selected { "› " } else { "  " };
                let line = Line::from(vec![
                    Span::styled(
                        format!("{marker}{:<12}", entry.provider_id),
                        if i == self.selected {
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Cyan)
                        },
                    ),
                    Span::styled(
                        entry.model.clone(),
                        if i == self.selected {
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Gray)
                        },
                    ),
                ]);
                ListItem::new(line)
            })
            .collect();
        List::new(items).render(chunks[1], buf);

        Paragraph::new(Line::from(Span::styled(
            "↑/↓ navigate · Enter switch · Esc cancel",
            Style::default().fg(Color::DarkGray),
        )))
        .render(chunks[2], buf);
    }
}

impl Default for ModelPicker {
    fn default() -> Self {
        Self::new()
    }
}

/// Curated catalog. Order roughly reflects "first try this" preferences.
fn catalog() -> Vec<ModelChoice> {
    let entries: &[(&str, &[&str])] = &[
        (
            "anthropic",
            &[
                "claude-opus-4-7",
                "claude-sonnet-4-6",
                "claude-haiku-4-5-20251001",
            ],
        ),
        (
            "openai",
            &["gpt-4.1", "gpt-4o", "gpt-4o-mini", "o4-mini"],
        ),
        (
            "openrouter",
            &[
                "anthropic/claude-opus-4-7",
                "anthropic/claude-sonnet-4-6",
                "openai/gpt-4.1",
                "google/gemini-2.5-pro",
                "meta-llama/llama-3.1-70b-instruct",
            ],
        ),
        (
            "gemini",
            &["gemini-2.5-pro", "gemini-2.5-flash", "gemini-1.5-pro"],
        ),
        (
            "litellm",
            &[
                "anthropic/claude-opus-4-7",
                "openai/gpt-4.1",
                "google/gemini-2.5-pro",
            ],
        ),
        ("lmstudio", &["local-model"]),
        ("vllm", &["local-model"]),
        ("ollama", &["llama3.1:8b", "qwen2.5:7b", "deepseek-r1:8b"]),
    ];
    let mut out = Vec::new();
    for (provider, models) in entries {
        for m in *models {
            out.push(ModelChoice {
                provider_id: (*provider).into(),
                model: (*m).into(),
            });
        }
    }
    out
}
