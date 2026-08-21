//! Cards: the durable half of the board.
//!
//! A card is a goal a human wrote. It is created in Backlog, dispatched into
//! one or more pilot runs over its life, and archived when it stops mattering.
//! Nothing about a card's *execution* lives here — that is read from the run.

use crate::store::{new_id, now, BoardError, BoardStore, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub id: String,
    pub project_id: String,
    pub title: String,
    /// Prompt handed to `pilot run`. Falls back to the title when empty.
    pub goal: String,
    pub notes: Option<String>,
    pub labels: Vec<String>,
    pub ord: f64,
    pub archived: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl Card {
    /// What `pilot run` is actually given.
    pub fn prompt(&self) -> &str {
        if self.goal.trim().is_empty() {
            &self.title
        } else {
            &self.goal
        }
    }

    /// The prefix a human types to identify this card.
    pub fn short(&self) -> &str {
        &self.id[..self.id.len().min(6)]
    }
}

/// Fields accepted when creating a card.
#[derive(Debug, Clone, Default)]
pub struct NewCard {
    pub project_id: String,
    pub title: String,
    pub goal: Option<String>,
    pub notes: Option<String>,
    pub labels: Vec<String>,
}

/// One dispatch of a card into a pilot run.
#[derive(Debug, Clone, PartialEq)]
pub struct Dispatch {
    pub id: i64,
    pub card_id: String,
    pub project_id: String,
    pub run_id: String,
    pub run_dir: std::path::PathBuf,
    pub started_at: String,
    pub ended_at: Option<String>,
}

impl Dispatch {
    pub fn is_live(&self) -> bool {
        self.ended_at.is_none()
    }
}

impl BoardStore {
    pub fn create_card(&self, new: NewCard) -> Result<Card> {
        if new.title.trim().is_empty() {
            return Err(BoardError::Invalid("card title cannot be empty".into()));
        }
        // New cards land at the top of Backlog. Descending `ord` means one
        // statement here instead of renumbering every sibling row.
        let ord: f64 = self
            .lock()
            .query_row(
                "SELECT COALESCE(MIN(ord), 0.0) - 1.0 FROM card WHERE project_id = ?1",
                [&new.project_id],
                |r| r.get(0),
            )
            .unwrap_or(0.0);

        let card = Card {
            id: new_id(),
            project_id: new.project_id,
            title: new.title.trim().to_string(),
            goal: new.goal.unwrap_or_default().trim().to_string(),
            notes: new.notes,
            labels: normalize_labels(new.labels),
            ord,
            archived: false,
            created_at: now(),
            updated_at: now(),
        };

        self.lock().execute(
            "INSERT INTO card (id, project_id, title, goal, notes, labels, ord, archived, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9)",
            (
                &card.id,
                &card.project_id,
                &card.title,
                &card.goal,
                &card.notes,
                card.labels.join(","),
                card.ord,
                &card.created_at,
                &card.updated_at,
            ),
        )?;
        Ok(card)
    }

    /// Cards for a project, or every project when `project_id` is `None`.
    pub fn cards(&self, project_id: Option<&str>, include_archived: bool) -> Result<Vec<Card>> {
        let conn = self.lock();
        let mut sql = String::from(
            "SELECT id, project_id, title, goal, notes, labels, ord, archived, created_at, updated_at
             FROM card WHERE 1 = 1",
        );
        if !include_archived {
            sql.push_str(" AND archived = 0");
        }
        if project_id.is_some() {
            sql.push_str(" AND project_id = ?1");
        }
        sql.push_str(" ORDER BY ord, created_at");

        let mut stmt = conn.prepare(&sql)?;
        let map = |r: &rusqlite::Row<'_>| {
            Ok(Card {
                id: r.get(0)?,
                project_id: r.get(1)?,
                title: r.get(2)?,
                goal: r.get(3)?,
                notes: r.get(4)?,
                labels: split_labels(&r.get::<_, String>(5)?),
                ord: r.get(6)?,
                archived: r.get::<_, i64>(7)? != 0,
                created_at: r.get(8)?,
                updated_at: r.get(9)?,
            })
        };
        let rows = match project_id {
            Some(p) => stmt.query_map([p], map)?.collect::<Vec<_>>(),
            None => stmt.query_map([], map)?.collect::<Vec<_>>(),
        };
        rows.into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Resolve a card by id or unique prefix. Prefixes shorter than 4
    /// characters are refused — a two-character prefix that happens to be
    /// unique today silently starts matching something else tomorrow.
    pub fn find_card(&self, prefix: &str) -> Result<Card> {
        let prefix = prefix.trim().to_ascii_lowercase();
        if prefix.len() < 4 {
            return Err(BoardError::Invalid(format!(
                "card id `{prefix}` is too short — use at least 4 characters"
            )));
        }
        let mut hits: Vec<Card> = self
            .cards(None, true)?
            .into_iter()
            .filter(|c| c.id.starts_with(&prefix))
            .collect();
        match hits.len() {
            0 => Err(BoardError::NoSuchCard(prefix)),
            1 => Ok(hits.remove(0)),
            _ => Err(BoardError::AmbiguousCard {
                prefix,
                candidates: hits
                    .iter()
                    .map(|c| format!("{} ({})", c.short(), c.title))
                    .collect(),
            }),
        }
    }

    pub fn update_card(&self, id: &str, title: Option<&str>, goal: Option<&str>) -> Result<()> {
        if let Some(t) = title {
            if t.trim().is_empty() {
                return Err(BoardError::Invalid("card title cannot be empty".into()));
            }
            self.lock()
                .execute("UPDATE card SET title = ?1 WHERE id = ?2", (t.trim(), id))?;
        }
        if let Some(g) = goal {
            self.lock()
                .execute("UPDATE card SET goal = ?1 WHERE id = ?2", (g.trim(), id))?;
        }
        self.touch_card(id)
    }

    pub fn set_archived(&self, id: &str, archived: bool) -> Result<()> {
        let n = self.lock().execute(
            "UPDATE card SET archived = ?1, updated_at = ?2 WHERE id = ?3",
            (i64::from(archived), now(), id),
        )?;
        if n == 0 {
            return Err(BoardError::NoSuchCard(id.to_string()));
        }
        Ok(())
    }

    /// Hard delete. Dispatch rows cascade; the runs themselves are untouched
    /// and stay visible in `pilot watch`.
    pub fn delete_card(&self, id: &str) -> Result<()> {
        let n = self
            .lock()
            .execute("DELETE FROM card WHERE id = ?1", [id])?;
        if n == 0 {
            return Err(BoardError::NoSuchCard(id.to_string()));
        }
        Ok(())
    }

    fn touch_card(&self, id: &str) -> Result<()> {
        self.lock()
            .execute("UPDATE card SET updated_at = ?1 WHERE id = ?2", (now(), id))?;
        Ok(())
    }

    // ---- dispatch history -------------------------------------------------

    pub fn record_dispatch(
        &self,
        card_id: &str,
        project_id: &str,
        run_id: &str,
        run_dir: &std::path::Path,
    ) -> Result<()> {
        self.lock().execute(
            "INSERT INTO dispatch (card_id, project_id, run_id, run_dir, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(project_id, run_id) DO NOTHING",
            (
                card_id,
                project_id,
                run_id,
                run_dir.to_string_lossy().to_string(),
                now(),
            ),
        )?;
        self.touch_card(card_id)
    }

    /// Dispatches for a card, newest first.
    pub fn dispatches(&self, card_id: &str) -> Result<Vec<Dispatch>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, card_id, project_id, run_id, run_dir, started_at, ended_at
             FROM dispatch WHERE card_id = ?1 ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([card_id], |r| {
            Ok(Dispatch {
                id: r.get(0)?,
                card_id: r.get(1)?,
                project_id: r.get(2)?,
                run_id: r.get(3)?,
                run_dir: std::path::PathBuf::from(r.get::<_, String>(4)?),
                started_at: r.get(5)?,
                ended_at: r.get(6)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn newest_dispatch(&self, card_id: &str) -> Result<Option<Dispatch>> {
        Ok(self.dispatches(card_id)?.into_iter().next())
    }

    /// Close a dispatch out. Called when its run reaches a terminal status or
    /// its run directory disappears.
    pub fn end_dispatch(&self, dispatch_id: i64) -> Result<()> {
        self.lock().execute(
            "UPDATE dispatch SET ended_at = ?1 WHERE id = ?2 AND ended_at IS NULL",
            (now(), dispatch_id),
        )?;
        Ok(())
    }
}

fn normalize_labels(labels: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = labels
        .into_iter()
        .flat_map(|l| {
            l.split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

fn split_labels(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::store;

    fn seeded() -> (tempfile::TempDir, BoardStore, String) {
        let (dir, s) = store();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let pid = s.touch_project(&root).unwrap();
        (dir, s, pid)
    }

    fn add(s: &BoardStore, pid: &str, title: &str) -> Card {
        s.create_card(NewCard {
            project_id: pid.to_string(),
            title: title.to_string(),
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn create_then_list_round_trips() {
        let (_d, s, pid) = seeded();
        let c = s
            .create_card(NewCard {
                project_id: pid.clone(),
                title: "  Fix LSP  ".into(),
                goal: Some("restart storm on save".into()),
                notes: Some("seen on windows".into()),
                labels: vec!["Bug".into(), "lsp,bug".into()],
            })
            .unwrap();

        assert_eq!(c.title, "Fix LSP", "title is trimmed");
        assert_eq!(c.labels, vec!["bug", "lsp"], "labels dedupe and sort");

        let all = s.cards(Some(&pid), false).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], c);
    }

    #[test]
    fn prompt_falls_back_to_title() {
        let (_d, s, pid) = seeded();
        let c = add(&s, &pid, "Just a title");
        assert_eq!(c.prompt(), "Just a title");

        let c2 = s
            .create_card(NewCard {
                project_id: pid,
                title: "T".into(),
                goal: Some("the real goal".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(c2.prompt(), "the real goal");
    }

    #[test]
    fn empty_title_is_refused() {
        let (_d, s, pid) = seeded();
        assert!(s
            .create_card(NewCard {
                project_id: pid,
                title: "   ".into(),
                ..Default::default()
            })
            .is_err());
    }

    #[test]
    fn prefix_resolution() {
        let (_d, s, pid) = seeded();
        let c = add(&s, &pid, "One");

        assert_eq!(s.find_card(&c.id).unwrap().id, c.id);
        assert_eq!(s.find_card(&c.id[..5]).unwrap().id, c.id);
        // Too short to be stable.
        assert!(s.find_card(&c.id[..3]).is_err());
        assert!(matches!(
            s.find_card("zzzzzzzz"),
            Err(BoardError::NoSuchCard(_))
        ));
    }

    #[test]
    fn ambiguous_prefix_lists_candidates() {
        let (_d, s, pid) = seeded();
        // Two cards sharing a prefix, inserted directly so the ids collide.
        for (id, title) in [("abcd1111aaaa", "First"), ("abcd2222bbbb", "Second")] {
            s.lock()
                .execute(
                    "INSERT INTO card (id, project_id, title, goal, labels, ord, created_at, updated_at)
                     VALUES (?1, ?2, ?3, '', '', 0.0, '', '')",
                    (id, &pid, title),
                )
                .unwrap();
        }
        match s.find_card("abcd") {
            Err(BoardError::AmbiguousCard { candidates, .. }) => {
                assert_eq!(candidates.len(), 2);
            }
            other => panic!("expected ambiguity, got {other:?}"),
        }
    }

    #[test]
    fn archive_hides_from_default_list() {
        let (_d, s, pid) = seeded();
        let c = add(&s, &pid, "One");

        s.set_archived(&c.id, true).unwrap();
        assert!(s.cards(Some(&pid), false).unwrap().is_empty());
        assert_eq!(s.cards(Some(&pid), true).unwrap().len(), 1);

        s.set_archived(&c.id, false).unwrap();
        assert_eq!(s.cards(Some(&pid), false).unwrap().len(), 1);
    }

    #[test]
    fn newest_card_sorts_first() {
        let (_d, s, pid) = seeded();
        add(&s, &pid, "older");
        add(&s, &pid, "newer");
        let titles: Vec<_> = s
            .cards(Some(&pid), false)
            .unwrap()
            .into_iter()
            .map(|c| c.title)
            .collect();
        assert_eq!(titles, vec!["newer", "older"]);
    }

    #[test]
    fn dispatch_history_is_newest_first_and_deduped() {
        let (dir, s, pid) = seeded();
        let c = add(&s, &pid, "One");
        let rd = dir.path().join("run1");

        s.record_dispatch(&c.id, &pid, "run1", &rd).unwrap();
        // Same (project, run) twice must not double-insert.
        s.record_dispatch(&c.id, &pid, "run1", &rd).unwrap();
        s.record_dispatch(&c.id, &pid, "run2", &dir.path().join("run2"))
            .unwrap();

        let hist = s.dispatches(&c.id).unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].run_id, "run2", "newest first");
        assert!(hist[0].is_live());

        s.end_dispatch(hist[0].id).unwrap();
        assert!(!s.newest_dispatch(&c.id).unwrap().unwrap().is_live());
    }

    #[test]
    fn delete_cascades_dispatches() {
        let (dir, s, pid) = seeded();
        let c = add(&s, &pid, "One");
        s.record_dispatch(&c.id, &pid, "run1", &dir.path().join("r"))
            .unwrap();

        s.delete_card(&c.id).unwrap();
        assert!(s.dispatches(&c.id).unwrap().is_empty());
        assert!(s.delete_card(&c.id).is_err());
    }
}
