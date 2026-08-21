//! Column derivation.
//!
//! There is no `column` field anywhere. A card's column is a pure function of
//! its newest dispatch and that run's roll-up, so the board cannot disagree
//! with `pilot watch` — both read the same `state.json`.
//!
//! `Failed` and `Blocked` are badges, not columns. At goal level a run is
//! usually mixed, and one failed task out of seven must not drag a card out of
//! In Progress while six others are still working.

use serde::{Deserialize, Serialize};
use wingman_autonomous::RunStatus;

use crate::card::Card;
use crate::rollup::Rollup;
use crate::store::{BoardStore, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    Backlog,
    Planned,
    InProgress,
    Review,
    Done,
}

impl Column {
    pub const ALL: [Column; 5] = [
        Column::Backlog,
        Column::Planned,
        Column::InProgress,
        Column::Review,
        Column::Done,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Column::Backlog => "BACKLOG",
            Column::Planned => "PLANNED",
            Column::InProgress => "IN PROGRESS",
            Column::Review => "REVIEW",
            Column::Done => "DONE",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Column::Backlog => "backlog",
            Column::Planned => "planned",
            Column::InProgress => "in-progress",
            Column::Review => "review",
            Column::Done => "done",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace([' ', '_'], "-")
            .as_str()
        {
            "backlog" => Some(Column::Backlog),
            "planned" => Some(Column::Planned),
            "in-progress" | "inprogress" | "running" => Some(Column::InProgress),
            "review" => Some(Column::Review),
            "done" => Some(Column::Done),
            _ => None,
        }
    }
}

/// The normative rule. See `docs/BOARD-SPEC.md` §4.3.
pub fn column_of(rollup: Option<&Rollup>) -> Column {
    let Some(r) = rollup else {
        return Column::Backlog;
    };
    match r.status {
        RunStatus::Planning | RunStatus::AwaitingApproval => Column::Planned,
        RunStatus::Done | RunStatus::Failed | RunStatus::Aborted => Column::Done,
        RunStatus::Running | RunStatus::Merging => {
            // Everything that can still move has parked in review.
            let settled = r.done + r.review + r.failed;
            if r.review > 0 && settled >= r.total {
                Column::Review
            } else {
                Column::InProgress
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Badge {
    /// `3/7`
    Progress {
        done: usize,
        total: usize,
    },
    /// `$1.24`
    Cost(f64),
    Failed(usize),
    Blocked(usize),
    Aborted,
    /// A task has been retried.
    Retry,
    /// The run directory or the project root is gone.
    Missing,
    Label(String),
    /// `+2` more labels than were rendered.
    MoreLabels(usize),
}

impl Badge {
    pub fn text(&self) -> String {
        match self {
            Badge::Progress { done, total } => format!("{done}/{total}"),
            Badge::Cost(u) => format!("${u:.2}"),
            Badge::Failed(n) => format!("!{n} failed"),
            Badge::Blocked(n) => format!("x{n} blocked"),
            Badge::Aborted => "aborted".into(),
            Badge::Retry => "retry".into(),
            Badge::Missing => "missing".into(),
            Badge::Label(l) => l.clone(),
            Badge::MoreLabels(n) => format!("+{n}"),
        }
    }
}

/// A card joined to its newest dispatch — what the renderer and `--json`
/// consume.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardCard {
    pub card: Card,
    pub project_name: String,
    pub project_missing: bool,
    pub run_id: Option<String>,
    pub rollup: Option<Rollup>,
    pub column: Column,
    pub badges: Vec<Badge>,
}

const MAX_LABELS: usize = 2;

fn badges_for(card: &Card, rollup: Option<&Rollup>, missing: bool) -> Vec<Badge> {
    let mut out = Vec::new();
    if let Some(r) = rollup {
        out.push(Badge::Progress {
            done: r.done,
            total: r.total,
        });
        if r.usd > 0.0 {
            out.push(Badge::Cost(r.usd));
        }
        if r.failed > 0 {
            out.push(Badge::Failed(r.failed));
        }
        if r.blocked > 0 {
            out.push(Badge::Blocked(r.blocked));
        }
        if r.status == RunStatus::Aborted {
            out.push(Badge::Aborted);
        }
        if r.retried() {
            out.push(Badge::Retry);
        }
    }
    if missing {
        out.push(Badge::Missing);
    }
    for l in card.labels.iter().take(MAX_LABELS) {
        out.push(Badge::Label(l.clone()));
    }
    if card.labels.len() > MAX_LABELS {
        out.push(Badge::MoreLabels(card.labels.len() - MAX_LABELS));
    }
    out
}

impl BoardStore {
    /// Build the whole board: every non-archived card of every visible
    /// project, joined to its newest dispatch and placed in a column.
    ///
    /// Closes out dispatches whose run directory has vanished, so a deleted
    /// run returns its card to Backlog rather than orphaning it.
    pub fn board(&self, project_id: Option<&str>) -> Result<Vec<BoardCard>> {
        let projects = self.projects(true)?;
        let visible: std::collections::HashMap<&str, &crate::Project> = projects
            .iter()
            .filter(|p| !p.hidden)
            .map(|p| (p.id.as_str(), p))
            .collect();

        let mut out = Vec::new();
        for card in self.cards(project_id, false)? {
            let Some(project) = visible.get(card.project_id.as_str()) else {
                continue; // forgotten project: its cards stay, hidden with it
            };

            let dispatch = self.newest_dispatch(&card.id)?;
            let mut run_id = None;
            let mut rollup = None;

            if let Some(d) = &dispatch {
                run_id = Some(d.run_id.clone());
                match self.rollup_for(&d.run_dir)? {
                    Some(r) => {
                        if r.is_terminal() && d.is_live() {
                            self.end_dispatch(d.id)?;
                        }
                        rollup = Some(r);
                    }
                    None => {
                        // Run directory gone: close it out, card falls back to
                        // Backlog on this very render.
                        if d.is_live() {
                            self.end_dispatch(d.id)?;
                        }
                        run_id = None;
                    }
                }
            }

            let missing = !project.exists() || (dispatch.is_some() && rollup.is_none());
            let column = column_of(rollup.as_ref());
            let badges = badges_for(&card, rollup.as_ref(), missing);
            out.push(BoardCard {
                card,
                project_name: project.name.clone(),
                project_missing: !project.exists(),
                run_id,
                rollup,
                column,
                badges,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollup::Rollup;

    fn rollup(
        status: RunStatus,
        done: usize,
        review: usize,
        failed: usize,
        total: usize,
    ) -> Rollup {
        Rollup {
            status,
            done,
            total,
            failed,
            blocked: 0,
            review,
            usd: 0.0,
            subrows: Vec::new(),
        }
    }

    #[test]
    fn no_dispatch_is_backlog() {
        assert_eq!(column_of(None), Column::Backlog);
    }

    #[test]
    fn planning_and_approval_are_planned() {
        for s in [RunStatus::Planning, RunStatus::AwaitingApproval] {
            assert_eq!(column_of(Some(&rollup(s, 0, 0, 0, 3))), Column::Planned);
        }
    }

    #[test]
    fn every_terminal_status_is_done() {
        for s in [RunStatus::Done, RunStatus::Failed, RunStatus::Aborted] {
            assert_eq!(column_of(Some(&rollup(s, 1, 0, 0, 3))), Column::Done);
        }
    }

    #[test]
    fn running_with_work_left_is_in_progress() {
        // 3 done of 7, nothing in review.
        let r = rollup(RunStatus::Running, 3, 0, 0, 7);
        assert_eq!(column_of(Some(&r)), Column::InProgress);
    }

    #[test]
    fn review_boundary() {
        // Everything settled and something is in review -> Review.
        let r = rollup(RunStatus::Running, 5, 2, 0, 7);
        assert_eq!(column_of(Some(&r)), Column::Review);

        // One task still working -> not yet.
        let r = rollup(RunStatus::Running, 4, 2, 0, 7);
        assert_eq!(column_of(Some(&r)), Column::InProgress);

        // Settled, but nothing in review (all done, merge pending) -> not Review.
        let r = rollup(RunStatus::Merging, 7, 0, 0, 7);
        assert_eq!(column_of(Some(&r)), Column::InProgress);

        // A failure counts as settled.
        let r = rollup(RunStatus::Running, 4, 2, 1, 7);
        assert_eq!(column_of(Some(&r)), Column::Review);
    }

    #[test]
    fn blocked_never_changes_the_column() {
        let mut r = rollup(RunStatus::Running, 0, 0, 0, 7);
        r.blocked = 7;
        assert_eq!(column_of(Some(&r)), Column::InProgress);
    }

    #[test]
    fn column_parse_round_trips() {
        for c in Column::ALL {
            assert_eq!(Column::parse(c.as_str()), Some(c));
        }
        assert_eq!(Column::parse("IN PROGRESS"), Some(Column::InProgress));
        assert_eq!(Column::parse("nope"), None);
    }

    #[test]
    fn badge_text() {
        assert_eq!(Badge::Progress { done: 3, total: 7 }.text(), "3/7");
        assert_eq!(Badge::Cost(1.239).text(), "$1.24");
        assert_eq!(Badge::Failed(2).text(), "!2 failed");
        assert_eq!(Badge::Blocked(1).text(), "x1 blocked");
        assert_eq!(Badge::MoreLabels(3).text(), "+3");
    }

    #[test]
    fn badges_cover_the_spec_table() {
        let card = Card {
            id: "abcdef123456".into(),
            project_id: "p".into(),
            title: "T".into(),
            goal: String::new(),
            notes: None,
            labels: vec!["a".into(), "b".into(), "c".into()],
            ord: 0.0,
            archived: false,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let mut r = rollup(RunStatus::Aborted, 3, 0, 2, 7);
        r.blocked = 1;
        r.usd = 1.5;
        r.subrows.push(crate::rollup::SubRow {
            task_id: "t1".into(),
            title: "t".into(),
            status: wingman_autonomous::TaskStatus::Done,
            role: "developer".into(),
            agent_name: None,
            model: None,
            session_id: None,
            usd: 0.0,
            attempts: 2,
            blocked_by: vec![],
            current_tool: None,
            deps: vec![],
            writes: 0,
            elapsed_secs: None,
            outcome: None,
            worktree: None,
        });

        let badges = badges_for(&card, Some(&r), true);
        let texts: Vec<String> = badges.iter().map(Badge::text).collect();
        assert!(texts.contains(&"3/7".to_string()));
        assert!(texts.contains(&"$1.50".to_string()));
        assert!(texts.contains(&"!2 failed".to_string()));
        assert!(texts.contains(&"x1 blocked".to_string()));
        assert!(texts.contains(&"aborted".to_string()));
        assert!(texts.contains(&"retry".to_string()));
        assert!(texts.contains(&"missing".to_string()));
        assert!(texts.contains(&"+1".to_string()), "3 labels, 2 shown");
    }
}
