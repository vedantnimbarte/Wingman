//! `/rewind` — the checkpoint timeline, one point per turn.
//!
//! Lists the project's checkpoints grouped by the turn that made them (see
//! `wingman_core::checkpoint::timeline`), newest first. Enter previews what
//! restoring to before a point would change — file by file, with the diff —
//! and `y` confirms. The restore is itself a checkpoint, so it appears at the
//! top of this list and is undone the same way.
//!
//! The whole project's timeline is shown, not only this session's: a restore
//! puts back every file touched since the point, whichever session touched
//! it, and the preview says so. Truncating the conversation (`t`) is offered
//! only on this session's own turns, and is off until asked for.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap},
};
use wingman_core::checkpoint::{self, Change};

use super::{centered_rect, ModalOutcome};

/// One point on the timeline, ready to draw.
#[derive(Debug, Clone)]
pub struct RewindRow {
    pub seq: u64,
    pub label: String,
    pub files: Vec<String>,
    /// This session's turn index, when truncating to it is possible.
    pub turn: Option<usize>,
}

/// What the user confirmed.
#[derive(Debug, Clone, PartialEq)]
pub struct RewindChoice {
    pub seq: u64,
    /// Also truncate the conversation to before this turn.
    pub truncate: Option<usize>,
}

#[derive(Debug)]
struct Confirm {
    changes: Result<Vec<Change>, String>,
    truncate: bool,
    scroll: u16,
}

#[derive(Debug)]
pub struct RewindView {
    root: PathBuf,
    rows: Vec<RewindRow>,
    selected: usize,
    confirm: Option<Confirm>,
    choice: Option<RewindChoice>,
}

impl RewindView {
    /// Read the timeline for `root`, labelling `session`'s turns with their
    /// prompts from its transcript.
    pub fn new(root: &Path, session: &str) -> Self {
        let sessions_dir = root.join(".wingman").join("sessions");
        let turns = wingman_session::session_path(&sessions_dir, session)
            .and_then(|p| wingman_session::load_session(&p).ok())
            .map(|r| wingman_session::turn_starts(&r))
            .unwrap_or_default();
        let rows = checkpoint::timeline(root)
            .into_iter()
            .map(|p| {
                let mine = p.session.as_deref() == Some(session);
                let label = match (p.restore, p.turn) {
                    (Some(target), _) => format!("restore to before #{target}"),
                    (None, Some(t)) if mine => {
                        let prompt = turns.get(t).map(|s| s.prompt.as_str()).unwrap_or("");
                        format!("turn {}: {}", t + 1, one_line(prompt, 60))
                    }
                    (None, Some(t)) => format!(
                        "turn {} of session {}",
                        t + 1,
                        p.session.as_deref().unwrap_or("")
                    ),
                    (None, None) => "edits outside a session".to_string(),
                };
                let turn = p
                    .turn
                    .filter(|&t| mine && p.restore.is_none() && t < turns.len());
                RewindRow {
                    seq: p.seq,
                    label,
                    files: p.files,
                    turn,
                }
            })
            .collect();
        Self {
            root: root.to_path_buf(),
            rows,
            selected: 0,
            confirm: None,
            choice: None,
        }
    }

    pub fn take_choice(&mut self) -> Option<RewindChoice> {
        self.choice.take()
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        let Some(confirm) = self.confirm.as_mut() else {
            match k.code {
                KeyCode::Up if self.selected > 0 => self.selected -= 1,
                KeyCode::Down if self.selected + 1 < self.rows.len() => self.selected += 1,
                KeyCode::Enter => {
                    if let Some(row) = self.rows.get(self.selected) {
                        self.confirm = Some(Confirm {
                            changes: checkpoint::preview(&self.root, row.seq),
                            truncate: false,
                            scroll: 0,
                        });
                    }
                }
                _ => {}
            }
            return ModalOutcome::Continue;
        };
        let row = &self.rows[self.selected];
        match k.code {
            KeyCode::Char('t') if row.turn.is_some() => confirm.truncate = !confirm.truncate,
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                // Nothing to restore and nothing to truncate: confirming
                // would do nothing, so it does not close as if it had.
                let changes = confirm.changes.as_ref().map(|c| c.len()).unwrap_or(0);
                if confirm.changes.is_ok() && (changes > 0 || confirm.truncate) {
                    self.choice = Some(RewindChoice {
                        seq: row.seq,
                        truncate: row.turn.filter(|_| confirm.truncate),
                    });
                    return ModalOutcome::Close;
                }
            }
            KeyCode::Char('n') | KeyCode::Backspace | KeyCode::Left => self.confirm = None,
            KeyCode::Up => confirm.scroll = confirm.scroll.saturating_sub(1),
            KeyCode::Down => confirm.scroll = confirm.scroll.saturating_add(1),
            _ => {}
        }
        ModalOutcome::Continue
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let rect = centered_rect(area, 85, 85);
        Clear.render(rect, buf);
        let title = match &self.confirm {
            None => " Rewind — checkpoints by turn ".to_string(),
            Some(_) => format!(" Restore to before: {} ", self.rows[self.selected].label),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(rect);
        block.render(rect, buf);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(inner);
        let hint = Style::default().fg(Color::DarkGray);

        let Some(confirm) = &self.confirm else {
            if self.rows.is_empty() {
                Paragraph::new("No checkpoints yet. Each file edit the agent makes adds one.")
                    .render(chunks[0], buf);
                return;
            }
            let lines: Vec<Line> = self
                .rows
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let style = if i == self.selected {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let marker = if i == self.selected { "› " } else { "  " };
                    Line::from(vec![
                        Span::styled(format!("{marker}{}", r.label), style),
                        Span::styled(format!("  · {}", files_summary(&r.files)), hint),
                    ])
                })
                .collect();
            // Keep the selection on screen in a long timeline.
            let height = chunks[0].height as usize;
            let scroll = self.selected.saturating_sub(height.saturating_sub(1)) as u16;
            Paragraph::new(lines)
                .scroll((scroll, 0))
                .render(chunks[0], buf);
            Paragraph::new(Line::from(Span::styled(
                "↑/↓ choose · Enter preview a restore to before this point · Esc close",
                hint,
            )))
            .render(chunks[1], buf);
            return;
        };

        let mut lines: Vec<Line> = Vec::new();
        match &confirm.changes {
            Err(e) => lines.push(Line::from(Span::styled(
                format!("Cannot restore: {e}"),
                Style::default().fg(Color::Red),
            ))),
            Ok(changes) if changes.is_empty() => lines.push(Line::from(
                "The files are already as they were at this point.",
            )),
            Ok(changes) => {
                lines.push(Line::from(format!(
                    "{} file(s) will change. The restore is checkpointed, so it can be undone from this timeline.",
                    changes.len()
                )));
                for c in changes {
                    let what = match (c.exists_now, c.exists_after) {
                        (true, false) => "removed",
                        (false, true) => "recreated",
                        _ => "restored",
                    };
                    lines.push(Line::from(Span::styled(
                        format!("  {what} {}", c.path),
                        Style::default().add_modifier(Modifier::BOLD),
                    )));
                }
                for c in changes {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        c.path.clone(),
                        Style::default().fg(Color::Cyan),
                    )));
                    for l in c.diff.lines() {
                        let style = if l.starts_with('+') {
                            Style::default().fg(Color::Green)
                        } else if l.starts_with('-') {
                            Style::default().fg(Color::Red)
                        } else if l.starts_with("@@") {
                            Style::default().fg(Color::Cyan)
                        } else {
                            Style::default()
                        };
                        lines.push(Line::from(Span::styled(l.to_string(), style)));
                    }
                }
            }
        }
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((confirm.scroll, 0))
            .render(chunks[0], buf);

        let mut footer = vec![Span::raw(" y restore · n back · ↑/↓ scroll")];
        if self.rows[self.selected].turn.is_some() {
            footer.push(Span::raw(format!(
                " · t also truncate the conversation to before this turn [{}]",
                if confirm.truncate { "x" } else { " " }
            )));
        }
        Paragraph::new(Line::from(footer)).render(chunks[1], buf);
    }
}

fn one_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else {
        line.to_string()
    }
}

fn files_summary(files: &[String]) -> String {
    match files {
        [one] => one.clone(),
        [first, second] => format!("{first}, {second}"),
        [first, second, rest @ ..] => format!("{first}, {second} +{} more", rest.len()),
        [] => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    /// One edit to `a.txt`, tagged as `session`'s turn. Serialized: the tag is
    /// process-wide, so two tests tagging at once would swap tags.
    fn tagged_edit(root: &Path, session: &str, turn: usize, content: &str) {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        checkpoint::set_turn(session, turn);
        let pre = checkpoint::capture(root, "a.txt");
        std::fs::write(root.join("a.txt"), content).unwrap();
        checkpoint::commit(root, vec![pre]);
    }

    #[test]
    fn previews_then_confirms_a_restore_with_an_explicit_truncate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = "20260101T000000000Z";
        let sessions = root.join(".wingman").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join(format!("{id}.jsonl")),
            "{\"kind\":\"user\",\"ts\":\"t\",\"text\":\"make it v1\"}\n\
             {\"kind\":\"stop\",\"ts\":\"t\",\"reason\":\"end_turn\"}\n",
        )
        .unwrap();
        std::fs::write(root.join("a.txt"), "v0\n").unwrap();
        tagged_edit(root, id, 0, "v1\n");

        let mut view = RewindView::new(root, id);
        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].label, "turn 1: make it v1");
        assert_eq!(view.rows[0].turn, Some(0));

        // `y` means nothing until a preview is on screen.
        view.handle_key(key(KeyCode::Char('y')));
        assert!(view.take_choice().is_none());

        view.handle_key(key(KeyCode::Enter));
        let changes = view.confirm.as_ref().unwrap().changes.as_ref().unwrap();
        assert!(changes[0].diff.contains("+v0"));

        view.handle_key(key(KeyCode::Char('t')));
        assert!(matches!(
            view.handle_key(key(KeyCode::Char('y'))),
            ModalOutcome::Close
        ));
        assert_eq!(
            view.take_choice(),
            Some(RewindChoice {
                seq: view.rows[0].seq,
                truncate: Some(0)
            })
        );
        // The preview wrote nothing.
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "v1\n");
    }

    #[test]
    fn another_sessions_turn_cannot_truncate_this_conversation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        tagged_edit(root, "someone-else", 2, "x");

        let mut view = RewindView::new(root, "this-session");
        assert_eq!(view.rows[0].label, "turn 3 of session someone-else");
        view.handle_key(key(KeyCode::Enter));
        view.handle_key(key(KeyCode::Char('t')));
        view.handle_key(key(KeyCode::Char('y')));
        assert_eq!(view.take_choice().unwrap().truncate, None);
    }
}
