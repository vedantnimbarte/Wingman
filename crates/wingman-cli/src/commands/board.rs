//! `wingman board …` — the kanban board over pilot runs.
//!
//! The headless half. Every subcommand here works without a terminal, so the
//! board is scriptable (`board list --json`, `board dispatch`) before the TUI
//! exists at all. See `docs/BOARD.md`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use wingman_board::{BoardCard, BoardStore, Column, DispatchOpts, NewCard};
use wingman_config::ProjectPaths;

/// Open the board and register the current project.
///
/// Auto-registration lives here rather than at each call site so every board
/// and pilot entry point picks it up for free.
pub fn open() -> Result<BoardStore> {
    let store = BoardStore::open_default().context("opening ~/.wingman/board.db")?;
    if let Ok(cwd) = std::env::current_dir() {
        let project = ProjectPaths::discover(&cwd);
        // Best-effort: a read-only home must not stop the board from listing.
        if let Err(e) = store.touch_project(&project.root) {
            tracing::debug!(target: "board", "project registration skipped: {e}");
        }
    }
    Ok(store)
}

/// Register `root` without opening anything else. Called from the pilot
/// commands so any repo you run pilot in shows up on the board.
pub fn touch(root: &Path) {
    let Ok(store) = BoardStore::open_default() else {
        return;
    };
    if let Err(e) = store.touch_project(root) {
        tracing::debug!(target: "board", "project registration skipped: {e}");
    }
}

fn current_project(store: &BoardStore, explicit: Option<String>) -> Result<String> {
    if let Some(id) = explicit {
        return Ok(store.project(&id)?.id);
    }
    let cwd = std::env::current_dir()?;
    let root = ProjectPaths::discover(&cwd).root;
    Ok(store.touch_project(&root)?)
}

pub async fn add(
    title: String,
    goal: Option<String>,
    project: Option<String>,
    label: Vec<String>,
    notes: Option<String>,
) -> Result<ExitCode> {
    let store = open()?;
    let project_id = current_project(&store, project)?;
    let card = store.create_card(NewCard {
        project_id,
        title,
        goal,
        notes,
        labels: label,
    })?;
    println!("{}  {}", card.short(), card.title);
    println!("dispatch it with: wingman board dispatch {}", card.short());
    Ok(ExitCode::SUCCESS)
}

pub async fn list(
    project: Option<String>,
    column: Option<String>,
    label: Option<String>,
    all: bool,
    json: bool,
) -> Result<ExitCode> {
    let store = open()?;
    let project_id = match project {
        Some(id) => Some(store.project(&id)?.id),
        None => None,
    };
    let want = match column.as_deref() {
        Some(c) => Some(Column::parse(c).with_context(|| {
            format!("unknown column `{c}` — use backlog|planned|in-progress|review|done")
        })?),
        None => None,
    };

    let mut cards = store.board(project_id.as_deref())?;
    if all {
        // `board()` only returns live cards; archived ones are listed flat.
        for c in store.cards(project_id.as_deref(), true)? {
            if c.archived && !cards.iter().any(|b| b.card.id == c.id) {
                cards.push(BoardCard {
                    project_name: store
                        .project(&c.project_id)
                        .map(|p| p.name)
                        .unwrap_or_default(),
                    project_missing: false,
                    card: c,
                    run_id: None,
                    rollup: None,
                    column: Column::Backlog,
                    badges: Vec::new(),
                });
            }
        }
    }
    if let Some(w) = want {
        cards.retain(|c| c.column == w);
    }
    if let Some(l) = &label {
        let l = l.to_ascii_lowercase();
        cards.retain(|c| c.card.labels.contains(&l));
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&to_json(&cards))?);
        return Ok(ExitCode::SUCCESS);
    }

    if cards.is_empty() {
        if want.is_some() || label.is_some() || project_id.is_some() {
            println!("no cards match that filter");
        } else {
            println!("no cards. add one with: wingman board add \"<title>\"");
        }
        return Ok(ExitCode::SUCCESS);
    }
    for col in Column::ALL {
        let in_col: Vec<&BoardCard> = cards.iter().filter(|c| c.column == col).collect();
        if in_col.is_empty() {
            continue;
        }
        println!("\n{} ({})", col.title(), in_col.len());
        for c in in_col {
            let badges = c
                .badges
                .iter()
                .map(|b| b.text())
                .collect::<Vec<_>>()
                .join(" · ");
            let archived = if c.card.archived { " [archived]" } else { "" };
            println!(
                "  {}  {:<40} {:<12} {}{}",
                c.card.short(),
                truncate(&c.card.title, 40),
                truncate(&c.project_name, 12),
                badges,
                archived
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn show(card: String) -> Result<ExitCode> {
    let store = open()?;
    let card = store.find_card(&card)?;
    let project = store.project(&card.project_id)?;

    println!("{}  {}", card.short(), card.title);
    println!("project   {} ({})", project.name, project.root.display());
    if !card.goal.trim().is_empty() {
        println!("goal      {}", card.goal);
    }
    if let Some(n) = &card.notes {
        println!("notes     {n}");
    }
    if !card.labels.is_empty() {
        println!("labels    {}", card.labels.join(", "));
    }
    println!("created   {}", card.created_at);

    let history = store.dispatches(&card.id)?;
    if history.is_empty() {
        println!("\nnot dispatched yet");
        return Ok(ExitCode::SUCCESS);
    }

    println!("\ndispatches ({})", history.len());
    for d in &history {
        let state = if d.is_live() { "live" } else { "ended" };
        println!("  {}  {}  {}", d.run_id, state, d.started_at);
    }

    if let Some(newest) = history.first() {
        if let Some(r) = store.rollup_for(&newest.run_dir)? {
            println!(
                "\nnewest run {}  {:?}  {}/{} done  ${:.2}",
                newest.run_id, r.status, r.done, r.total, r.usd
            );
            for s in &r.subrows {
                let agent = s.agent_name.as_deref().unwrap_or("--");
                let model = s.model.as_deref().unwrap_or("--");
                let blocked = if s.blocked_by.is_empty() {
                    String::new()
                } else {
                    format!("  dep {}", s.blocked_by.join(","))
                };
                // Derived `Debug` ignores width specifiers, so the status has
                // to become a String before it can be padded into a column.
                let status = format!("{:?}", s.status);
                println!(
                    "  {:<6} {:<11} {:<28} {:<14} {:<18} ${:.2}{}",
                    s.task_id,
                    status,
                    truncate(&s.title, 28),
                    agent,
                    model,
                    s.usd,
                    blocked
                );
                // Second line only when there is something to say — keeps a
                // clean run to one line per task.
                let mut notes = Vec::new();
                if s.attempts > 1 {
                    notes.push(format!("{} attempts", s.attempts));
                }
                if let Some(secs) = s.elapsed_secs {
                    notes.push(fmt_dur(secs));
                }
                if s.writes > 0 {
                    notes.push(format!("{} path(s)", s.writes));
                }
                if let Some(o) = &s.outcome {
                    notes.push(o.clone());
                }
                if !notes.is_empty() {
                    println!("         {}", notes.join("  ·  "));
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn dispatch(card: String, again: bool, pilot_flags: Vec<String>) -> Result<ExitCode> {
    let store = open()?;
    let card = store.find_card(&card)?;
    let project = store.project(&card.project_id)?;

    let out = store.dispatch_card(
        &card,
        &project,
        &DispatchOpts {
            extra_args: pilot_flags,
            again,
        },
    )?;
    println!("dispatched {} -> run {}", card.short(), out.run_id);
    println!("watch:  wingman pilot watch {}", out.run_id);
    Ok(ExitCode::SUCCESS)
}

pub async fn archive(card: String, restore: bool) -> Result<ExitCode> {
    let store = open()?;
    let card = store.find_card(&card)?;
    store.set_archived(&card.id, !restore)?;
    println!(
        "{} {}",
        if restore { "restored" } else { "archived" },
        card.short()
    );
    Ok(ExitCode::SUCCESS)
}

pub async fn rm(card: String, yes: bool) -> Result<ExitCode> {
    let store = open()?;
    let card = store.find_card(&card)?;
    let history = store.dispatches(&card.id)?;
    if !history.is_empty() && !yes {
        eprintln!(
            "wingman: {} has {} dispatch(es). Re-run with --yes to delete it.",
            card.short(),
            history.len()
        );
        eprintln!("         The runs themselves are not deleted.");
        return Ok(ExitCode::from(1));
    }
    store.delete_card(&card.id)?;
    println!("deleted {}", card.short());
    Ok(ExitCode::SUCCESS)
}

pub async fn projects(
    forget: Option<String>,
    restore: Option<String>,
    relocate: Option<Vec<String>>,
    all: bool,
) -> Result<ExitCode> {
    let store = open()?;
    if let Some(id) = forget {
        store.forget_project(&id)?;
        println!("forgot {id} (its cards are kept; --restore brings it back)");
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(id) = restore {
        store.restore_project(&id)?;
        println!("restored {id}");
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(args) = relocate {
        let [id, path] = args.as_slice() else {
            anyhow::bail!("--relocate takes exactly ID and PATH");
        };
        store.relocate_project(id, &PathBuf::from(path))?;
        println!("relocated {id} -> {path}");
        return Ok(ExitCode::SUCCESS);
    }

    let projects = store.projects(all)?;
    if projects.is_empty() {
        println!("no projects registered yet — run any wingman pilot or board command in a repo");
        return Ok(ExitCode::SUCCESS);
    }
    for p in projects {
        let mut flags = Vec::new();
        if p.hidden {
            flags.push("hidden");
        }
        if !p.exists() {
            flags.push("missing");
        }
        println!(
            "{:<20} {:<50} {}",
            p.id,
            truncate(&p.root.to_string_lossy(), 50),
            flags.join(" ")
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn export(json: bool) -> Result<ExitCode> {
    let store = open()?;
    let cards = store.cards(None, true)?;
    if !json {
        println!("{} card(s) in {}", cards.len(), store.path().display());
        return Ok(ExitCode::SUCCESS);
    }
    let mut out = Vec::new();
    for c in cards {
        let history = store.dispatches(&c.id)?;
        out.push(serde_json::json!({
            "id": c.id,
            "project": c.project_id,
            "title": c.title,
            "goal": c.goal,
            "notes": c.notes,
            "labels": c.labels,
            "archived": c.archived,
            "created_at": c.created_at,
            "updated_at": c.updated_at,
            "dispatches": history.iter().map(|d| serde_json::json!({
                "run_id": d.run_id,
                "run_dir": d.run_dir,
                "started_at": d.started_at,
                "ended_at": d.ended_at,
            })).collect::<Vec<_>>(),
        }));
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(ExitCode::SUCCESS)
}

fn to_json(cards: &[BoardCard]) -> Vec<serde_json::Value> {
    cards
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.card.id,
                "short": c.card.short(),
                "title": c.card.title,
                "goal": c.card.goal,
                "labels": c.card.labels,
                "archived": c.card.archived,
                "project": c.card.project_id,
                "project_name": c.project_name,
                "project_missing": c.project_missing,
                "column": c.column.as_str(),
                "run_id": c.run_id,
                "badges": c.badges.iter().map(|b| b.text()).collect::<Vec<_>>(),
                "rollup": c.rollup,
            })
        })
        .collect()
}

/// Human-readable duration, matching the board TUI's detail overlay.
fn fmt_dur(secs: i64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let (m, s) = (secs / 60, secs % 60);
    if m < 60 {
        return format!("{m}m{s:02}s");
    }
    format!("{}h{:02}m", m / 60, m % 60)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('~');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd~");
        // Multi-byte input must not panic or split a char.
        assert_eq!(truncate("héllo wörld", 6), "héllo~");
    }
}
