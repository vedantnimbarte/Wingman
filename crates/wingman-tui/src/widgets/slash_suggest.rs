//! Inline slash-command autocomplete that floats above the composer.
//!
//! Unlike modals, the suggester does **not** capture all input — the user
//! keeps typing into the composer, and the popup just narrows itself.
//! Only Up/Down/Tab are intercepted while the popup is visible.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Widget},
};

/// Static command catalog — keep aliases out so suggestions stay tidy.
pub struct Command {
    pub name: &'static str,
    pub description: &'static str,
}

pub const CATALOG: &[Command] = &[
    Command {
        name: "/help",
        description: "show command list",
    },
    Command {
        name: "/clear",
        description: "reset the conversation",
    },
    Command {
        name: "/login",
        description: "connect a provider (guided wizard)",
    },
    Command {
        name: "/logout",
        description: "remove a stored API key",
    },
    Command {
        name: "/model",
        description: "switch model · empty arg opens the picker",
    },
    Command {
        name: "/mode",
        description: "switch permission mode · empty arg opens the picker",
    },
    Command {
        name: "/add",
        description: "attach a file to the next prompt",
    },
    Command {
        name: "/usage",
        description: "token + cost breakdown by provider (`clear` to reset)",
    },
    Command {
        name: "/skills",
        description: "browse and apply skills",
    },
    Command {
        name: "/skill",
        description: "queue a named skill for next prompt",
    },
    Command {
        name: "/memory",
        description: "list saved memories · forget <name>",
    },
    Command {
        name: "/recall",
        description: "search across past sessions",
    },
    Command {
        name: "/learn",
        description: "self-learning status / reset",
    },
    Command {
        name: "/mcp",
        description: "manage MCP servers",
    },
    Command {
        name: "/find",
        description: "search this transcript · /findnext, /findprev",
    },
    Command {
        name: "/quit",
        description: "exit wingman",
    },
];

/// Drives the popup state. Keep in sync with the composer via
/// [`update`] after every keystroke.
#[derive(Debug, Default)]
pub struct SlashSuggest {
    /// Indices into [`CATALOG`] in display order.
    matches: Vec<usize>,
    selected: usize,
}

impl SlashSuggest {
    /// Recompute matches from the current composer text. Visible when the
    /// input starts with `/` and the user hasn't yet typed a space (i.e.
    /// we're still naming the command, not entering an argument).
    pub fn update(&mut self, input: &str) {
        let trimmed = input.trim_start();
        if !trimmed.starts_with('/') || trimmed.contains(char::is_whitespace) {
            self.matches.clear();
            self.selected = 0;
            return;
        }
        let needle = trimmed.to_ascii_lowercase();
        let mut next: Vec<usize> = CATALOG
            .iter()
            .enumerate()
            .filter(|(_, c)| c.name.starts_with(&needle))
            .map(|(i, _)| i)
            .collect();
        if next.is_empty() && needle == "/" {
            // Bare "/" shows everything.
            next = (0..CATALOG.len()).collect();
        }
        if self.selected >= next.len() {
            self.selected = 0;
        }
        self.matches = next;
    }

    pub fn is_visible(&self) -> bool {
        !self.matches.is_empty()
    }

    pub fn move_up(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.matches.len() - 1;
        } else {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.matches.len();
    }

    /// Currently-highlighted command name, e.g. `"/login"`.
    pub fn selected_command(&self) -> Option<&'static str> {
        self.matches.get(self.selected).map(|&i| CATALOG[i].name)
    }

    /// Render above the composer. The caller chooses the rect.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if !self.is_visible() {
            return;
        }
        Clear.render(area, buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " /",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        block.render(area, buf);

        let items: Vec<ListItem> = self
            .matches
            .iter()
            .enumerate()
            .map(|(row, &idx)| {
                let cmd = &CATALOG[idx];
                let selected = row == self.selected;
                let marker_style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Cyan)
                };
                let name_style = if selected {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                let desc_style = Style::default().fg(Color::DarkGray);
                let marker = if selected { "› " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{marker}{:<14}", cmd.name),
                        name_style.patch(marker_style),
                    ),
                    Span::styled(cmd.description, desc_style),
                ]))
            })
            .collect();
        List::new(items).render(inner, buf);
    }

    /// Suggested rendered height in rows (incl. borders), capped to a sane
    /// max so a long catalog doesn't swallow the chat.
    pub fn rendered_height(&self) -> u16 {
        if !self.is_visible() {
            return 0;
        }
        let rows = self.matches.len().min(6) as u16; // cap visible rows
        rows + 2 // top + bottom border
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_hidden() {
        let mut s = SlashSuggest::default();
        s.update("");
        assert!(!s.is_visible());
    }

    #[test]
    fn bare_slash_lists_all() {
        let mut s = SlashSuggest::default();
        s.update("/");
        assert_eq!(s.matches.len(), CATALOG.len());
    }

    #[test]
    fn prefix_filters() {
        let mut s = SlashSuggest::default();
        s.update("/lo");
        let names: Vec<_> = s.matches.iter().map(|&i| CATALOG[i].name).collect();
        assert!(names.contains(&"/login"));
        assert!(names.contains(&"/logout"));
        assert!(!names.contains(&"/clear"));
    }

    #[test]
    fn space_hides_popup() {
        let mut s = SlashSuggest::default();
        s.update("/model anthropic");
        assert!(!s.is_visible());
    }
}
