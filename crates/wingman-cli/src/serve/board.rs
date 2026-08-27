//! Board routes — the kanban board over pilot runs, as JSON.
//!
//! The board is **global**: one `~/.wingman/board.db` spanning every project,
//! not a per-repo file. That is why these routes are hand-written here beside
//! `/v1/config` rather than added to `table.rs`, whose routes are
//! project-scoped without exception.
//!
//! `wingman-board` is called **in-process**, not shelled out to. It is already
//! a dependency of this crate, the derivation it exposes is headless by
//! design, and there is a specific bug waiting for anyone who reaches for a
//! subprocess instead: `board dispatch` once used `Command::output()`, which
//! waits for stdout to reach EOF rather than for the launcher to exit, so it
//! blocked for the entire run while a detached grandchild held the pipes
//! (BOARD-PLAN.md § Found by the first live run). `dispatch_card` carries the
//! fix. A fresh shell-out would re-earn the bug.
//!
//! Column, roll-up and badges all come from `store.board()` — the same
//! derivation `wingman board` renders — so the panel and the TUI cannot
//! disagree about what state a card is in.
//!
//! Queries run synchronously on the connection task. That matches how the
//! pilot routes already read `state.json` off disk, and a local SQLite read
//! measured in microseconds does not earn a `spawn_blocking` round trip.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use wingman_board::{BoardCard, BoardError, BoardStore, Column, DispatchOpts, NewCard};

use super::http::{self, Request};
use super::ServeState;

/// Open the global board.
///
/// Deliberately **not** `commands::board::open`, which also calls
/// `touch_project(cwd)` to auto-register the repo you are standing in. The
/// daemon's cwd is wherever it happened to be launched — often not a repo at
/// all — and registering it as a board project would put a phantom column
/// header in front of every user who ever ran `wingman serve` from `~`.
fn open() -> Result<BoardStore, BoardError> {
    BoardStore::open_default()
}

/// Map a board error onto the status it actually is.
///
/// An ambiguous card prefix is the caller's problem to fix by being more
/// specific, so it is a `400` carrying the candidates rather than a `404` that
/// says the card does not exist when several do.
async fn fail(sock: &mut TcpStream, e: BoardError) -> std::io::Result<()> {
    let status = match &e {
        BoardError::NoSuchCard(_) | BoardError::NoSuchProject(_) => 404,
        BoardError::AmbiguousCard { .. } | BoardError::Invalid(_) => 400,
        BoardError::Sql(_) | BoardError::Io { .. } | BoardError::Config(_) => 500,
    };
    http::write_err(sock, status, &e.to_string()).await
}

macro_rules! or_fail {
    ($sock:expr, $expr:expr) => {
        match $expr {
            Ok(v) => v,
            Err(e) => return fail($sock, e).await,
        }
    };
}

/// Register the daemon's allowlisted repos in the board registry, once.
///
/// Without this a fresh `wingman serve` user opens the panel onto an empty
/// board and cannot add a card, because adding one needs a project that only
/// gets registered by running pilot from a terminal — the exact terminal trip
/// the panel exists to avoid.
///
/// `import_serve_projects` is guarded by a `meta` key rather than by the
/// registry being empty, so someone who deliberately forgets every imported
/// project does not get them all back on the next restart.
pub fn import_projects(state: &ServeState) {
    let roots: Vec<PathBuf> = state.projects.iter().map(|p| p.root.clone()).collect();
    // Best-effort, like the CLI's own registration: a read-only home or a
    // locked database must not stop the daemon from starting.
    match open().and_then(|s| s.import_serve_projects(&roots)) {
        Ok(0) => {}
        Ok(n) => tracing::info!(target: "serve", "board: registered {n} project(s)"),
        Err(e) => tracing::debug!(target: "serve", "board: project import skipped: {e}"),
    }
}

pub async fn route(
    state: &Arc<ServeState>,
    req: &Request,
    rest: &[&str],
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    match (req.method.as_str(), rest) {
        ("GET", []) => get_board(req, sock).await,
        ("GET", ["projects"]) => get_projects(sock).await,
        ("POST", ["cards"]) => add_card(req, sock).await,
        ("GET", ["cards", id]) => get_card(id, sock).await,
        ("PATCH", ["cards", id]) => update_card(id, req, sock).await,
        ("POST", ["cards", id, "dispatch"]) => dispatch_card(state, id, req, sock).await,
        ("POST", ["cards", id, "archive"]) => archive_card(id, req, sock).await,
        ("DELETE", ["cards", id]) => delete_card(id, sock).await,
        _ => http::write_err(sock, 404, "no such board route (see GET /v1/schema)").await,
    }
}

/// `GET /v1/board` — every live card, with its derived column and roll-up.
///
/// Projects ride along in the same response because the panel needs both to
/// render one frame, and two round trips would let the card list and the
/// project list disagree about a repo that vanished between them.
async fn get_board(req: &Request, sock: &mut TcpStream) -> std::io::Result<()> {
    let store = or_fail!(sock, open());
    let project = req.query_str("project");
    let mut cards = or_fail!(sock, store.board(project));

    if req.query_bool("archived") {
        // `board()` returns live cards only. Archived ones have no run to
        // derive from, so they are reported in Backlog with no roll-up rather
        // than being given a column that would imply progress.
        for c in or_fail!(sock, store.cards(project, true)) {
            if c.archived && !cards.iter().any(|b| b.card.id == c.id) {
                let project_name = store
                    .project(&c.project_id)
                    .map(|p| p.name)
                    .unwrap_or_default();
                cards.push(BoardCard {
                    project_name,
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

    let projects = or_fail!(sock, store.projects(false));
    http::write_json(
        sock,
        200,
        &json!({
            "columns": Column::ALL.iter().map(|c| json!({
                "id": c.as_str(),
                "title": c.title(),
            })).collect::<Vec<_>>(),
            "cards": cards.iter().map(card_json).collect::<Vec<_>>(),
            "projects": projects.iter().map(|p| json!({
                "id": p.id,
                "name": p.name,
                "root": p.root.to_string_lossy(),
                "missing": !p.exists(),
            })).collect::<Vec<_>>(),
        }),
    )
    .await
}

/// Which kind of badge this is, so a renderer can act on it without parsing
/// the display text.
///
/// `Badge::text()` alone is what `wingman board list --json` emits, and it is
/// lossy: `"0/3"` and `"$1.04"` are indistinguishable from a label a user
/// typed. A panel that renders progress and cost as structured fields needs to
/// know which badges it has already shown, and matching on formatted strings
/// would break the first time a formatter changed a decimal place.
fn badge_kind(b: &wingman_board::Badge) -> &'static str {
    use wingman_board::Badge::*;
    match b {
        Progress { .. } => "progress",
        Cost(_) => "cost",
        Failed(_) => "failed",
        Blocked(_) => "blocked",
        Aborted => "aborted",
        Retry => "retry",
        Missing => "missing",
        Label(_) => "label",
        MoreLabels(_) => "more_labels",
    }
}

/// The wire shape of one card.
///
/// Fields mirror `commands::board::to_json` except for `badges`, which carry
/// their kind here — see [`badge_kind`]. Everything else is identical, so the
/// panel and `wingman board list --json` describe a card the same way.
fn card_json(c: &BoardCard) -> Value {
    json!({
        "id": c.card.id,
        "short": c.card.short(),
        "title": c.card.title,
        "goal": c.card.goal,
        "notes": c.card.notes,
        "labels": c.card.labels,
        "archived": c.card.archived,
        "created_at": c.card.created_at,
        "project": c.card.project_id,
        "project_name": c.project_name,
        "project_missing": c.project_missing,
        "column": c.column.as_str(),
        "run_id": c.run_id,
        "badges": c.badges.iter()
            .map(|b| json!({ "kind": badge_kind(b), "text": b.text() }))
            .collect::<Vec<_>>(),
        "rollup": c.rollup,
    })
}

async fn get_projects(sock: &mut TcpStream) -> std::io::Result<()> {
    let store = or_fail!(sock, open());
    let projects = or_fail!(sock, store.projects(false));
    http::write_json(
        sock,
        200,
        &json!({
            "projects": projects.iter().map(|p| json!({
                "id": p.id,
                "name": p.name,
                "root": p.root.to_string_lossy(),
                "missing": !p.exists(),
            })).collect::<Vec<_>>(),
        }),
    )
    .await
}

async fn get_card(id: &str, sock: &mut TcpStream) -> std::io::Result<()> {
    let store = or_fail!(sock, open());
    let card = or_fail!(sock, store.find_card(id));
    let dispatches = or_fail!(sock, store.dispatches(&card.id));
    http::write_json(
        sock,
        200,
        &json!({
            "card": {
                "id": card.id,
                "short": card.short(),
                "title": card.title,
                "goal": card.goal,
                "notes": card.notes,
                "labels": card.labels,
                "archived": card.archived,
                "project": card.project_id,
                "created_at": card.created_at,
            },
            // The history that makes "this goal took three attempts" answerable.
            "dispatches": dispatches.iter().map(|d| json!({
                "run_id": d.run_id,
                "project": d.project_id,
                "started_at": d.started_at,
                "ended_at": d.ended_at,
            })).collect::<Vec<_>>(),
        }),
    )
    .await
}

#[derive(Deserialize)]
struct NewCardBody {
    project: String,
    title: String,
    goal: Option<String>,
    notes: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
}

async fn add_card(req: &Request, sock: &mut TcpStream) -> std::io::Result<()> {
    let body: NewCardBody = match req.json() {
        Ok(b) => b,
        Err(e) => return http::write_err(sock, 400, &e).await,
    };
    if body.title.trim().is_empty() {
        return http::write_err(sock, 400, "title is required").await;
    }

    let store = or_fail!(sock, open());
    // Resolve the project before creating anything, so a typo is a 404 rather
    // than a card stranded against a project id that does not exist.
    let project = or_fail!(sock, store.project(&body.project));
    let card = or_fail!(
        sock,
        store.create_card(NewCard {
            project_id: project.id,
            title: body.title.trim().to_string(),
            goal: body.goal,
            notes: body.notes,
            labels: body.labels,
        })
    );
    http::write_json(sock, 201, &json!({ "id": card.id, "short": card.short() })).await
}

#[derive(Deserialize, Default)]
struct DispatchBody {
    /// Dispatch even though a live dispatch already exists.
    #[serde(default)]
    again: bool,
    /// Extra `pilot run` flags, forwarded verbatim and validated by
    /// `plan_dispatch` — `--worker-mode`, `--detached` and `--watch` are
    /// refused there, not here, so the CLI and the API refuse the same set.
    #[serde(default)]
    args: Vec<String>,
}

/// `POST /v1/board/cards/{id}/dispatch` — start a pilot run for this card.
///
/// The run is spawned detached and this returns immediately with its id. It
/// does **not** pass through `state.ceiling`: the spawned pilot reads its own
/// permission settings from config exactly as `wingman board dispatch` does,
/// and the ceiling governs turns this daemon runs, not processes it launches.
async fn dispatch_card(
    state: &Arc<ServeState>,
    id: &str,
    req: &Request,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    let body: DispatchBody = req.json().unwrap_or_default();

    let store = or_fail!(sock, open());
    let card = or_fail!(sock, store.find_card(id));
    let project = or_fail!(sock, store.project(&card.project_id));

    if !dispatch_allowed(&state.projects, &project.root) {
        return http::write_err(
            sock,
            403,
            "that card's project is not in this server's allowlist",
        )
        .await;
    }

    let opts = DispatchOpts {
        extra_args: body.args,
        again: body.again,
    };
    let done = or_fail!(sock, store.dispatch_card(&card, &project, &opts));
    http::write_json(
        sock,
        202,
        &json!({
            "run_id": done.run_id,
            "project": project.id,
            "pid": done.pid,
        }),
    )
    .await
}

/// May this daemon dispatch a card whose project lives at `root`?
///
/// The board registry is **global** and accumulates every repo the user has
/// ever run pilot in. `serve`'s allowlist is deliberately narrower. Without
/// this check, holding the API token would let a request start an agent with
/// write access in any directory the board happens to remember — turning the
/// allowlist, which is the one boundary `serve` has, into a suggestion.
///
/// `projects::find` accepts a root and canonicalises both sides, so a registry
/// entry pointing at `/repo/.` or through a symlink resolves to the same
/// project rather than sneaking past as a different string.
fn dispatch_allowed(allowlist: &[super::projects::Project], root: &std::path::Path) -> bool {
    super::projects::find(allowlist, &root.to_string_lossy()).is_some()
}

#[derive(Deserialize, Default)]
struct ArchiveBody {
    /// Move it back out of the archive instead.
    #[serde(default)]
    restore: bool,
}

async fn archive_card(id: &str, req: &Request, sock: &mut TcpStream) -> std::io::Result<()> {
    let body: ArchiveBody = req.json().unwrap_or_default();
    let store = or_fail!(sock, open());
    let card = or_fail!(sock, store.find_card(id));
    or_fail!(sock, store.set_archived(&card.id, !body.restore));
    http::write_json(
        sock,
        200,
        &json!({ "id": card.id, "archived": !body.restore }),
    )
    .await
}

#[derive(Deserialize, Default)]
struct EditBody {
    title: Option<String>,
    goal: Option<String>,
}

/// `PATCH /v1/board/cards/{id}` — correct a card's title or goal.
///
/// A card is durable and outlives its runs, so the wording of a goal written
/// weeks ago is worth being able to fix without deleting the history that goal
/// produced. Only the keys present in the body are touched — an absent `goal`
/// leaves the stored one alone rather than clearing it, which is the
/// difference between an edit form that ships one field and one that silently
/// wipes the other.
async fn update_card(id: &str, req: &Request, sock: &mut TcpStream) -> std::io::Result<()> {
    let body: EditBody = match req.json() {
        Ok(b) => b,
        Err(e) => return http::write_err(sock, 400, &e).await,
    };
    if body.title.is_none() && body.goal.is_none() {
        return http::write_err(sock, 400, "nothing to change: send \"title\" or \"goal\"").await;
    }

    let store = or_fail!(sock, open());
    let card = or_fail!(sock, store.find_card(id));
    or_fail!(
        sock,
        store.update_card(&card.id, body.title.as_deref(), body.goal.as_deref())
    );
    let updated = or_fail!(sock, store.find_card(&card.id));
    http::write_json(
        sock,
        200,
        &json!({ "id": updated.id, "title": updated.title, "goal": updated.goal }),
    )
    .await
}

/// `DELETE /v1/board/cards/{id}` — forget a card and its dispatch history.
///
/// The runs themselves are untouched: they live in the project's
/// `.wingman/autonomous/`, this store never wrote them, and deleting a card
/// must not delete the record of work that actually happened.
async fn delete_card(id: &str, sock: &mut TcpStream) -> std::io::Result<()> {
    let store = or_fail!(sock, open());
    let card = or_fail!(sock, store.find_card(id));
    or_fail!(sock, store.delete_card(&card.id));
    http::write_raw(sock, 204, "application/json", &[], &[]).await
}

/// Routes for `GET /v1/schema`.
pub fn schema() -> Vec<Value> {
    vec![
        json!({ "method": "GET", "path": "/v1/board", "auth": true,
                "params": { "project": "filter to one project id",
                            "archived": "include archived cards" },
                "returns": "columns, cards with derived column/rollup/badges, and projects" }),
        json!({ "method": "GET", "path": "/v1/board/projects", "auth": true,
                "returns": "the board's project registry" }),
        json!({ "method": "POST", "path": "/v1/board/cards", "auth": true,
                "body": { "project": "string", "title": "string", "goal": "string?",
                          "notes": "string?", "labels": "string[]?" },
                "returns": "{id, short}" }),
        json!({ "method": "GET", "path": "/v1/board/cards/{card}", "auth": true,
                "returns": "one card and its dispatch history" }),
        json!({ "method": "PATCH", "path": "/v1/board/cards/{card}", "auth": true,
                "body": { "title": "string?", "goal": "string?" },
                "returns": "the card's new title and goal; absent keys are left alone" }),
        json!({ "method": "POST", "path": "/v1/board/cards/{card}/dispatch", "auth": true,
                "body": { "again": "bool", "args": "string[] — extra pilot run flags" },
                "returns": "{run_id, project, pid} — spawned detached" }),
        json!({ "method": "POST", "path": "/v1/board/cards/{card}/archive", "auth": true,
                "body": { "restore": "bool — unarchive instead" } }),
        json!({ "method": "DELETE", "path": "/v1/board/cards/{card}", "auth": true,
                "returns": "204; the card's runs on disk are untouched" }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// These deliberately avoid opening a store. `BoardStore::open_default`
    /// resolves to the real `~/.wingman/board.db`, and a test suite that
    /// creates cards there would edit the developer's own board.
    fn allowlist(roots: &[&Path]) -> Vec<super::super::projects::Project> {
        roots
            .iter()
            .enumerate()
            .map(|(i, r)| super::super::projects::Project {
                id: format!("p{i}"),
                root: r.canonicalize().unwrap(),
            })
            .collect()
    }

    #[test]
    fn a_project_outside_the_allowlist_cannot_be_dispatched() {
        let served = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let list = allowlist(&[served.path()]);

        assert!(dispatch_allowed(
            &list,
            &served.path().canonicalize().unwrap()
        ));
        assert!(!dispatch_allowed(&list, other.path()));
    }

    #[test]
    fn an_empty_allowlist_dispatches_nothing() {
        let any = tempfile::tempdir().unwrap();
        assert!(!dispatch_allowed(&[], any.path()));
    }

    /// A registry entry can name the same directory by a non-canonical path.
    /// It must resolve to the same project, not slip past as a new one.
    #[test]
    fn a_non_canonical_root_still_matches() {
        let served = tempfile::tempdir().unwrap();
        let list = allowlist(&[served.path()]);
        assert!(dispatch_allowed(&list, &served.path().join(".")));
    }

    #[test]
    fn board_errors_map_onto_the_status_they_actually_are() {
        let status = |e: &BoardError| match e {
            BoardError::NoSuchCard(_) | BoardError::NoSuchProject(_) => 404,
            BoardError::AmbiguousCard { .. } | BoardError::Invalid(_) => 400,
            BoardError::Sql(_) | BoardError::Io { .. } | BoardError::Config(_) => 500,
        };
        assert_eq!(status(&BoardError::NoSuchCard("a1".into())), 404);
        assert_eq!(status(&BoardError::NoSuchProject("x".into())), 404);
        assert_eq!(status(&BoardError::Invalid("bad flag".into())), 400);
        // Ambiguity is the caller being imprecise about a card that exists —
        // 404 would say the opposite of what happened.
        assert_eq!(
            status(&BoardError::AmbiguousCard {
                prefix: "a".into(),
                candidates: vec!["a1".into(), "a2".into()],
            }),
            400
        );
    }

    /// Every `Badge` variant must get a kind. A `_ => "other"` arm would
    /// compile forever and silently lump a new variant in with the ones the
    /// panel already knows how to hide.
    #[test]
    fn every_badge_kind_is_named() {
        use wingman_board::Badge;
        let all = [
            (Badge::Progress { done: 1, total: 3 }, "progress", "1/3"),
            (Badge::Cost(1.5), "cost", "$1.50"),
            (Badge::Failed(2), "failed", "!2 failed"),
            (Badge::Blocked(1), "blocked", "x1 blocked"),
            (Badge::Aborted, "aborted", "aborted"),
            (Badge::Retry, "retry", "retry"),
            (Badge::Missing, "missing", "missing"),
            (Badge::Label("test".into()), "label", "test"),
            (Badge::MoreLabels(2), "more_labels", "+2"),
        ];
        for (badge, kind, text) in all {
            assert_eq!(badge_kind(&badge), kind);
            assert_eq!(badge.text(), text);
        }
    }

    #[test]
    fn the_schema_lists_every_route_the_router_answers() {
        let paths: Vec<String> = schema()
            .iter()
            .map(|r| {
                format!(
                    "{} {}",
                    r["method"].as_str().unwrap(),
                    r["path"].as_str().unwrap()
                )
            })
            .collect();
        for expected in [
            "GET /v1/board",
            "GET /v1/board/projects",
            "POST /v1/board/cards",
            "GET /v1/board/cards/{card}",
            "POST /v1/board/cards/{card}/dispatch",
            "POST /v1/board/cards/{card}/archive",
            "DELETE /v1/board/cards/{card}",
        ] {
            assert!(
                paths.contains(&expected.to_string()),
                "{expected} missing from schema"
            );
        }
    }
}
