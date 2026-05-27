//! Toggleable left sidebar showing a flat file listing for the project.
//!
//! Kept intentionally simple: walks one level at a time and lists folders
//! and files alphabetically. `j`/`k` (or arrows) move; Enter inserts the
//! selected path into the composer; `Tab` enters a directory; Backspace
//! goes up.

use crate::theme;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Widget},
};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct FileTree {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

impl FileTree {
    pub fn new(root: PathBuf) -> Self {
        let cwd = root.clone();
        let mut me = Self {
            root,
            cwd,
            entries: Vec::new(),
            selected: 0,
        };
        me.refresh();
        me
    }

    pub fn refresh(&mut self) {
        let mut entries: Vec<Entry> = Vec::new();
        if self.cwd != self.root {
            entries.push(Entry {
                name: "..".into(),
                is_dir: true,
            });
        }
        if let Ok(rd) = std::fs::read_dir(&self.cwd) {
            for e in rd.flatten() {
                let Ok(ft) = e.file_type() else { continue };
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                entries.push(Entry {
                    name,
                    is_dir: ft.is_dir(),
                });
            }
        }
        entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });
        self.entries = entries;
        if self.selected >= self.entries.len() {
            self.selected = self.entries.len().saturating_sub(1);
        }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }
    pub fn move_down(&mut self) {
        if !self.entries.is_empty() && self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }
    /// Returns Some(path) if the user picked a file; descends into a
    /// directory and returns None.
    pub fn enter(&mut self) -> Option<PathBuf> {
        let Some(e) = self.entries.get(self.selected).cloned() else {
            return None;
        };
        if e.is_dir {
            if e.name == ".." {
                if let Some(parent) = self.cwd.parent() {
                    self.cwd = parent.to_path_buf();
                }
            } else {
                self.cwd = self.cwd.join(&e.name);
            }
            self.selected = 0;
            self.refresh();
            None
        } else {
            Some(self.cwd.join(&e.name))
        }
    }

    /// Pretty relative path from `root`. Falls back to absolute if escape.
    pub fn pick_relative(&self, p: &Path) -> String {
        p.strip_prefix(&self.root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| p.display().to_string())
    }
}

pub struct FileTreeView<'a> {
    pub tree: &'a FileTree,
}

impl<'a> Widget for FileTreeView<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let th = theme::current();
        let rel = self
            .tree
            .cwd
            .strip_prefix(&self.tree.root)
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".into());
        let title = if rel.is_empty() { "./".to_string() } else { format!("./{rel}") };
        let items: Vec<ListItem> = self
            .tree
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let mut spans = vec![
                    Span::raw(if e.is_dir { "▸ " } else { "  " }),
                    Span::raw(e.name.clone()),
                ];
                if e.is_dir {
                    spans.push(Span::raw("/"));
                }
                let style = if i == self.tree.selected {
                    Style::default()
                        .fg(th.user_prompt)
                        .add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else if e.is_dir {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(spans)).style(style)
            })
            .collect();
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {title} "));
        let list = List::new(items).block(block);
        let mut state = ListState::default();
        state.select(Some(self.tree.selected));
        ratatui::widgets::StatefulWidget::render(list, area, buf, &mut state);
    }
}
