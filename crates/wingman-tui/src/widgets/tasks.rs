//! Live task checklist panel, driven by the model's `update_tasks` tool.
//!
//! The TUI watches for `update_tasks` tool calls (see `app::apply_event`'s
//! call site) and replaces [`UiState::tasks`] with [`parse`]'d contents.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget, Wrap},
};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Done,
}

impl TaskStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "in_progress" => Self::InProgress,
            "done" => Self::Done,
            _ => Self::Pending,
        }
    }

    /// Checkbox glyph shown before the task text.
    fn glyph(self) -> &'static str {
        match self {
            Self::Pending => "☐",
            Self::InProgress => "◐",
            Self::Done => "☑",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Pending => Color::DarkGray,
            Self::InProgress => Color::Yellow,
            Self::Done => Color::Green,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaskItem {
    pub text: String,
    pub status: TaskStatus,
}

/// Parse the `update_tasks` tool input (`{ "tasks": [{text,status}, …] }`)
/// into a task list. Unknown/malformed entries are skipped, so a partial or
/// odd payload degrades gracefully rather than erroring.
pub fn parse(input: &Value) -> Vec<TaskItem> {
    input
        .get("tasks")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let text = t.get("text")?.as_str()?.to_string();
                    let status = TaskStatus::from_str(t.get("status").and_then(|s| s.as_str()).unwrap_or("pending"));
                    Some(TaskItem { text, status })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub struct TasksView<'a> {
    pub tasks: &'a [TaskItem],
}

impl<'a> Widget for TasksView<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let done = self.tasks.iter().filter(|t| t.status == TaskStatus::Done).count();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                format!(" Tasks {done}/{} ", self.tasks.len()),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        block.render(area, buf);

        let lines: Vec<Line> = self
            .tasks
            .iter()
            .map(|t| {
                let mut style = Style::default().fg(t.status.color());
                if t.status == TaskStatus::Done {
                    style = style.add_modifier(Modifier::CROSSED_OUT);
                } else if t.status == TaskStatus::InProgress {
                    style = style.add_modifier(Modifier::BOLD);
                }
                Line::from(vec![
                    Span::styled(format!("{} ", t.status.glyph()), Style::default().fg(t.status.color())),
                    Span::styled(t.text.clone(), style),
                ])
            })
            .collect();
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .render(inner, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_tasks_and_skips_garbage() {
        let v = json!({ "tasks": [
            { "text": "a", "status": "done" },
            { "text": "b", "status": "in_progress" },
            { "text": "c" },                 // missing status → pending
            { "status": "done" },            // missing text → skipped
        ]});
        let tasks = parse(&v);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].status, TaskStatus::Done);
        assert_eq!(tasks[1].status, TaskStatus::InProgress);
        assert_eq!(tasks[2].status, TaskStatus::Pending);
        assert!(parse(&json!({})).is_empty());
    }
}
