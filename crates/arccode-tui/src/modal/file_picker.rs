//! Fuzzy file picker — the `@` modal.
//!
//! Walks the project root once via [`ignore::WalkBuilder`] (respects
//! `.gitignore`, capped at 10k entries), then re-ranks against the query
//! on every keystroke with [`nucleo_matcher`].

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use ignore::WalkBuilder;
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

const MAX_FILES: usize = 10_000;
const VISIBLE_ROWS: usize = 12;

#[derive(Debug)]
pub struct FilePicker {
    /// All discovered files as paths relative to the project root, in walk
    /// order. The host expands these against its own root at send time.
    entries: Vec<String>,
    /// Indices into `entries`, ranked by current query. When the query is
    /// empty this is just `0..entries.len()` (capped to a sensible window).
    ranked: Vec<usize>,
    query: String,
    selected: usize,
    /// Indices into `entries` that have been toggled via Space.
    checked: std::collections::HashSet<usize>,
    truncated: bool,
}

impl FilePicker {
    /// Build a picker rooted at `root`. Walking happens here (synchronously);
    /// in a 10k-file project this takes a few ms.
    pub fn new(root: PathBuf) -> Self {
        let (entries, truncated) = walk(&root);
        let ranked: Vec<usize> = (0..entries.len()).collect();
        Self {
            entries,
            ranked,
            query: String::new(),
            selected: 0,
            checked: Default::default(),
            truncated,
        }
    }

    /// Path the user just confirmed, if any. Wizard-style: caller polls
    /// after a `handle_key` that returned [`ModalOutcome::Close`].
    pub fn take_selected(&mut self) -> Option<String> {
        self.ranked
            .get(self.selected)
            .and_then(|&i| self.entries.get(i))
            .cloned()
    }

    /// Returns all checked paths, or the single highlighted path if none checked.
    pub fn take_selected_all(&mut self) -> Vec<String> {
        if !self.checked.is_empty() {
            let mut result: Vec<String> = self.checked
                .iter()
                .filter_map(|&i| self.entries.get(i).cloned())
                .collect();
            result.sort();
            result
        } else {
            self.take_selected().into_iter().collect()
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        match k.code {
            KeyCode::Char(' ') => {
                if let Some(&idx) = self.ranked.get(self.selected) {
                    if self.checked.contains(&idx) {
                        self.checked.remove(&idx);
                    } else {
                        self.checked.insert(idx);
                    }
                }
            }
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
        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(usize, u32)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                buf.clear();
                let needle = Utf32Str::new(item, &mut buf);
                pattern.score(needle, &mut matcher).map(|s| (i, s))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        self.ranked = scored.into_iter().map(|(i, _)| i).collect();
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let rect = centered_rect(area, 70, 70);
        Clear.render(rect, buf);
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                format!(" @ — attach file ({} indexed) ", self.entries.len()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = outer.inner(rect);
        outer.render(rect, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // query line
                Constraint::Min(3),    // list
                Constraint::Length(2), // hint
            ])
            .split(inner);

        let query_line = Line::from(vec![
            Span::styled("filter: ", Style::default().fg(Color::DarkGray)),
            Span::styled(self.query.clone(), Style::default().fg(Color::White)),
            Span::raw("▏"),
        ]);
        Paragraph::new(query_line).render(chunks[0], buf);

        // Render a window around the selected entry so navigation past the
        // bottom keeps the cursor visible.
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
                let is_checked = self.checked.contains(&idx);
                let marker = match (i == self.selected, is_checked) {
                    (true, true)   => "›☑ ",
                    (true, false)  => "›  ",
                    (false, true)  => " ☑ ",
                    (false, false) => "   ",
                };
                let style = if i == self.selected {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(format!("{marker}{entry}"), style)))
            })
            .collect();
        List::new(items).render(chunks[1], buf);

        let hint = if self.truncated {
            format!(
                "↑/↓ navigate · Space toggle · Enter attach · Esc cancel · (capped at {MAX_FILES} files)"
            )
        } else {
            "↑/↓ navigate · Space toggle · Enter attach · Esc cancel".to_string()
        };
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        )))
        .render(chunks[2], buf);
    }
}

/// Walk `root` once, skipping ignored files. Returns the list and a flag
/// indicating whether we hit the cap.
fn walk(root: &Path) -> (Vec<String>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    // `hidden(true)` (the default) skips dot-prefixed files and directories
    // like `.git/`, `.env`, etc. The `git_*` knobs respect `.gitignore`,
    // `.git/info/exclude`, and the user's global git ignores.
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .parents(true)
        .build();
    for dent in walker.flatten() {
        if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = dent.path();
        let rel = path.strip_prefix(root).unwrap_or(path);
        if let Some(s) = rel.to_str() {
            // Normalize Windows backslashes to forward slashes so the
            // path round-trips cleanly through the prompt and the agent.
            out.push(s.replace('\\', "/"));
            if out.len() >= MAX_FILES {
                truncated = true;
                break;
            }
        }
    }
    out.sort();
    (out, truncated)
}
