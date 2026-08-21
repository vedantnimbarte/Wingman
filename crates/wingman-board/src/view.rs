//! Headless board layout.
//!
//! The renderer produces a [`BoardView`] of plain strings, and
//! [`BoardView::to_ascii_width`] lays it out as text. Nothing here needs a
//! terminal, so every layout decision — column widths, truncation, the narrow
//! fallback, sub-row indentation — is snapshot-testable. The ratatui half in
//! `wingman-cli` only draws what this produces.
//!
//! Mirrors `wingman_autonomous::dashboard::DashboardView::to_ascii_width`,
//! which does the same for `pilot status`.

use std::collections::HashSet;

use crate::column::{BoardCard, Column};

/// Below this many columns, five side-by-side boxes are unreadable slivers,
/// so the board degrades to a single grouped list instead.
pub const MIN_GRID_WIDTH: usize = 100;

/// Default width when the caller has no terminal size to offer.
const DEFAULT_WIDTH: usize = 120;

#[derive(Debug, Clone, PartialEq)]
pub struct SubRowView {
    pub task_id: String,
    /// Status glyph position — the caller picks the glyph.
    pub status: String,
    pub title: String,
    pub agent: String,
    pub model: String,
    pub usd: f64,
    pub blocked_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CardView {
    pub id: String,
    pub short: String,
    pub project: String,
    pub title: String,
    pub badges: Vec<String>,
    pub expanded: bool,
    pub subrows: Vec<SubRowView>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnView {
    pub column: Column,
    pub cards: Vec<CardView>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoardView {
    pub columns: Vec<ColumnView>,
}

impl BoardView {
    /// Group cards into columns. `expanded` holds the ids whose sub-rows
    /// should be shown.
    pub fn build(cards: &[BoardCard], expanded: &HashSet<String>) -> Self {
        let columns = Column::ALL
            .iter()
            .map(|&col| ColumnView {
                column: col,
                cards: cards
                    .iter()
                    .filter(|c| c.column == col)
                    .map(|c| card_view(c, expanded.contains(&c.card.id)))
                    .collect(),
            })
            .collect();
        BoardView { columns }
    }

    pub fn is_empty(&self) -> bool {
        self.columns.iter().all(|c| c.cards.is_empty())
    }

    pub fn total_cards(&self) -> usize {
        self.columns.iter().map(|c| c.cards.len()).sum()
    }

    pub fn to_ascii(&self) -> String {
        self.to_ascii_width(DEFAULT_WIDTH)
    }

    /// Lay the board out at an explicit total width.
    ///
    /// At or above [`MIN_GRID_WIDTH`] this is five side-by-side boxes; below
    /// it, a single list grouped by column header.
    pub fn to_ascii_width(&self, total: usize) -> String {
        if self.is_empty() {
            return "no cards\n".to_string();
        }
        if total < MIN_GRID_WIDTH {
            return self.to_narrow(total.max(20));
        }

        let n = self.columns.len();
        let w = total / n;
        let panes: Vec<Vec<String>> = self.columns.iter().map(|c| boxed(c, w)).collect();

        let rows = panes.iter().map(Vec::len).max().unwrap_or(0);
        let blank = " ".repeat(w);
        let mut s = String::new();
        for i in 0..rows {
            for pane in &panes {
                s.push_str(pane.get(i).map(String::as_str).unwrap_or(&blank));
            }
            // Trailing padding is invisible but makes diffs noisy.
            while s.ends_with(' ') {
                s.pop();
            }
            s.push('\n');
        }
        s
    }

    /// Single-column fallback for narrow terminals.
    fn to_narrow(&self, w: usize) -> String {
        let mut s = String::new();
        for col in &self.columns {
            if col.cards.is_empty() {
                continue;
            }
            s.push_str(&format!("{} ({})\n", col.column.title(), col.cards.len()));
            for card in &col.cards {
                for line in card_lines(card, w.saturating_sub(2)) {
                    s.push_str("  ");
                    s.push_str(line.trim_end());
                    s.push('\n');
                }
            }
            s.push('\n');
        }
        s
    }
}

fn card_view(c: &BoardCard, expanded: bool) -> CardView {
    CardView {
        id: c.card.id.clone(),
        short: c.card.short().to_string(),
        project: c.project_name.clone(),
        title: c.card.title.clone(),
        badges: c.badges.iter().map(|b| b.text()).collect(),
        expanded,
        subrows: match (&c.rollup, expanded) {
            (Some(r), true) => r
                .subrows
                .iter()
                .map(|s| SubRowView {
                    task_id: s.task_id.clone(),
                    status: format!("{:?}", s.status).to_lowercase(),
                    title: s.title.clone(),
                    agent: s.agent_name.clone().unwrap_or_else(|| "--".into()),
                    model: s.model.clone().unwrap_or_else(|| "--".into()),
                    usd: s.usd,
                    blocked_by: s.blocked_by.clone(),
                })
                .collect(),
            _ => Vec::new(),
        },
    }
}

/// The text lines one card occupies, at content width `w`.
fn card_lines(card: &CardView, w: usize) -> Vec<String> {
    let marker = if card.subrows.is_empty() {
        if card.expanded {
            'v'
        } else {
            '>'
        }
    } else {
        'v'
    };
    let mut out = vec![
        format!("{marker} {}", trunc(&card.project, w.saturating_sub(2))),
        format!("  {}", trunc(&card.title, w.saturating_sub(2))),
    ];
    if !card.badges.is_empty() {
        out.push(format!(
            "  {}",
            trunc(&card.badges.join(" "), w.saturating_sub(2))
        ));
    }
    for (i, s) in card.subrows.iter().enumerate() {
        let last = i + 1 == card.subrows.len();
        let branch = if last { '`' } else { '|' };
        out.push(format!(
            " {branch}- {} {}",
            s.task_id,
            trunc(&s.title, w.saturating_sub(4 + s.task_id.len()))
        ));
        let detail = if s.blocked_by.is_empty() {
            format!("{} {} ${:.2}", s.agent, s.model, s.usd)
        } else {
            format!("dep {}", s.blocked_by.join(","))
        };
        out.push(format!(
            " {}  {}",
            if last { ' ' } else { '|' },
            trunc(&detail, w.saturating_sub(4))
        ));
    }
    out
}

/// One column as a bordered box of exactly `w` columns per line.
fn boxed(col: &ColumnView, w: usize) -> Vec<String> {
    let inner = w.saturating_sub(2);
    let title = format!(" {} ({}) ", col.column.title(), col.cards.len());
    let title = trunc(&title, inner);
    let dashes = inner.saturating_sub(title.chars().count());

    let mut out = vec![format!("+{}{}+", title, "-".repeat(dashes))];
    for card in &col.cards {
        for line in card_lines(card, inner) {
            out.push(format!("|{}|", pad(&trunc(&line, inner), inner)));
        }
        out.push(format!("|{}|", " ".repeat(inner)));
    }
    out.push(format!("+{}+", "-".repeat(inner)));
    out
}

fn trunc(s: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('~');
    out
}

fn pad(s: &str, n: usize) -> String {
    let len = s.chars().count();
    if len >= n {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(n - len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Card;
    use crate::column::Badge;
    use crate::rollup::{Rollup, SubRow};
    use wingman_autonomous::{RunStatus, TaskStatus};

    fn card(title: &str, column: Column) -> BoardCard {
        BoardCard {
            card: Card {
                id: format!("{title}00000000000")[..12].to_string(),
                project_id: "wingman".into(),
                title: title.into(),
                goal: String::new(),
                notes: None,
                labels: vec![],
                ord: 0.0,
                archived: false,
                created_at: String::new(),
                updated_at: String::new(),
            },
            project_name: "Wingman".into(),
            project_missing: false,
            run_id: None,
            rollup: None,
            column,
            badges: vec![],
        }
    }

    fn with_run(mut c: BoardCard) -> BoardCard {
        c.run_id = Some("r1".into());
        c.badges = vec![Badge::Progress { done: 1, total: 2 }, Badge::Cost(1.5)];
        c.rollup = Some(Rollup {
            status: RunStatus::Running,
            done: 1,
            total: 2,
            failed: 0,
            blocked: 0,
            review: 0,
            usd: 1.5,
            subrows: vec![
                SubRow {
                    task_id: "t1".into(),
                    title: "implement".into(),
                    status: TaskStatus::Done,
                    role: "developer".into(),
                    agent_name: Some("brave_otter".into()),
                    model: Some("opus-5".into()),
                    session_id: None,
                    usd: 1.0,
                    attempts: 1,
                    blocked_by: vec![],
                    current_tool: None,
                    deps: vec![],
                    writes: 0,
                    elapsed_secs: None,
                    outcome: None,
                    worktree: None,
                },
                SubRow {
                    task_id: "t2".into(),
                    title: "tests".into(),
                    status: TaskStatus::Pending,
                    role: "tester".into(),
                    agent_name: None,
                    model: None,
                    session_id: None,
                    usd: 0.0,
                    attempts: 0,
                    blocked_by: vec!["t1".into()],
                    current_tool: None,
                    deps: vec!["t1".into()],
                    writes: 0,
                    elapsed_secs: None,
                    outcome: None,
                    worktree: None,
                },
            ],
        });
        c
    }

    fn no_expand() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn empty_board() {
        let v = BoardView::build(&[], &no_expand());
        assert!(v.is_empty());
        assert_eq!(v.to_ascii_width(120), "no cards\n");
    }

    #[test]
    fn one_card_per_column() {
        let cards: Vec<BoardCard> = Column::ALL
            .iter()
            .map(|&c| card(&format!("card{}", c.as_str()), c))
            .collect();
        let v = BoardView::build(&cards, &no_expand());
        assert_eq!(v.total_cards(), 5);
        for c in &v.columns {
            assert_eq!(c.cards.len(), 1, "{:?}", c.column);
        }

        let s = v.to_ascii_width(150);
        for col in Column::ALL {
            assert!(s.contains(col.title()), "missing {}", col.title());
        }
    }

    #[test]
    fn every_grid_line_fits_the_width() {
        let cards = vec![
            card(
                "a very long card title that will certainly overflow",
                Column::Backlog,
            ),
            with_run(card("running", Column::InProgress)),
        ];
        let mut ex = HashSet::new();
        ex.insert(cards[1].card.id.clone());
        let v = BoardView::build(&cards, &ex);

        for width in [100, 120, 160, 200] {
            for line in v.to_ascii_width(width).lines() {
                assert!(
                    line.chars().count() <= width,
                    "width {width}: {} chars in {line:?}",
                    line.chars().count()
                );
            }
        }
    }

    #[test]
    fn expanded_card_shows_subrows_with_model_and_deps() {
        let c = with_run(card("running", Column::InProgress));
        let mut ex = HashSet::new();
        ex.insert(c.card.id.clone());
        let v = BoardView::build(&[c], &ex);

        let s = v.to_ascii_width(160);
        assert!(s.contains("t1"), "task id missing:\n{s}");
        assert!(s.contains("brave_otter"), "agent missing:\n{s}");
        assert!(s.contains("opus-5"), "model missing:\n{s}");
        assert!(s.contains("dep t1"), "blocked-by missing:\n{s}");
    }

    #[test]
    fn collapsed_card_hides_subrows() {
        let c = with_run(card("running", Column::InProgress));
        let v = BoardView::build(&[c], &no_expand());
        let s = v.to_ascii_width(160);
        assert!(
            !s.contains("brave_otter"),
            "collapsed card leaked subrows:\n{s}"
        );
        assert!(s.contains("1/2"), "roll-up badge should still show:\n{s}");
    }

    #[test]
    fn narrow_terminal_falls_back_to_a_list() {
        let cards = vec![
            card("one", Column::Backlog),
            card("two", Column::InProgress),
        ];
        let v = BoardView::build(&cards, &no_expand());
        let s = v.to_ascii_width(60);

        assert!(!s.contains('+'), "narrow mode must not draw boxes:\n{s}");
        assert!(s.contains("BACKLOG (1)"));
        assert!(s.contains("IN PROGRESS (1)"));
        for line in s.lines() {
            assert!(line.chars().count() <= 60, "{line:?}");
        }
    }

    #[test]
    fn trunc_and_pad_are_char_safe() {
        assert_eq!(trunc("héllo", 10), "héllo");
        assert_eq!(trunc("héllo", 3), "hé~");
        assert_eq!(trunc("héllo", 0), "");
        assert_eq!(pad("ab", 5).chars().count(), 5);
        assert_eq!(pad("abcdef", 3), "abcdef");
    }
}
