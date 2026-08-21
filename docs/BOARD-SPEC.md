# Board — specification

Normative spec for `wingman board`. Architecture and rationale are in
[BOARD.md](BOARD.md); build order is in [BOARD-PLAN.md](BOARD-PLAN.md).

Everything here is written so a fresh session with no prior context can
implement a phase without re-deriving decisions.

---

## 1. Orientation — what already exists

| Area | File | Items the board consumes |
| --- | --- | --- |
| Run data model | `crates/wingman-autonomous/src/model.rs` | `Task`, `Agent`, `TaskStatus`, `RunStatus`, `AgentStatus`, `RunState`, `Event`, `apply` |
| Run reads | `crates/wingman-autonomous/src/dashboard.rs` | `list_runs`, `load_state`, `tail_events`, `state_mtime`, `RunSummary`, `TaskRow`, `AgentRow`, `DashboardView::to_ascii` |
| Run paths | `crates/wingman-autonomous/src/lib.rs` | `run_dir(project_root, run_id)` |
| Control channel | `crates/wingman-autonomous/src/control.rs` | `ControlCommand`, `append`, `control_path` |
| Watch TUI internals | `crates/wingman-cli/src/commands/pilot_watch_tui.rs` | `Focus`, `LogView`, `SevFilter`, `Glyphs`, `HitAreas`, detail/help overlays, confirm modal, toast |
| Global paths | `crates/wingman-config/src/paths.rs` | `global_dir`, `ensure_global_dir`, `find_project_root` |
| SQLite precedent | `crates/wingman-learn/src/stats.rs` | `StatsStore::open` — `Connection` + `execute_batch` + `Mutex` |
| Serve project list | `crates/wingman-config/src/lib.rs` | `ServeProject { id, root }`, `effective_id()` |

New code lives in a new crate `wingman-board`, depended on by `wingman-cli`.
It depends on `wingman-autonomous` (read-only), `wingman-config`, `rusqlite`,
`serde`, `chrono`, `rand` — every one already a workspace dependency, so the
board adds no third-party crate. It must not depend on `wingman-cli` or on
ratatui: rendering produces a plain `BoardView` that the CLI draws.

---

## 2. Prerequisite change: `Agent.model`

**File:** `crates/wingman-autonomous/src/model.rs`

Add to `Agent`:

```rust
/// Model id the worker last reported spending on, from `agent.usd`.
/// `None` for runs recorded before this field existed, and for the
/// in-process manager before its first priced turn.
#[serde(default)]
pub model: Option<String>,
```

Initialise to `None` wherever `Agent` is constructed. In `apply()`, under the
`Event::AgentUsd { agent, model, .. }` arm, set the agent's `model` when
`model` is non-empty. Last write wins — a worker that switches model mid-task
reports the most recent one, which is what the card should show.

**Compatibility:** `#[serde(default)]` means existing `state.json` snapshots
deserialise unchanged, and `tasks.jsonl` already carries `model` on every
`agent.usd` line, so replaying an old run backfills the field for free.

**Test:** apply a `RunStart` + `AgentSpawn` + `AgentUsd { model: "opus-5" }`
sequence and assert `state.agent("a1").unwrap().model.as_deref() ==
Some("opus-5")`; assert a state with no `AgentUsd` yields `None`.

---

## 3. Store: `~/.wingman/board.db`

Opened via `wingman_config::ensure_global_dir()?.join("board.db")`, following
`StatsStore::open`. Two deltas from that precedent, both required because the
board TUI is long-lived while `pilot` commands write the registry concurrently:

```rust
conn.execute_batch(
    "PRAGMA journal_mode = WAL;
     PRAGMA foreign_keys = ON;",
)?;
conn.busy_timeout(std::time::Duration::from_secs(5))?;
```

### 3.1 Schema

`schema_version` starts at `1` and lives in `meta`. Migrations are forward-only
`ALTER TABLE` steps guarded by the stored version.

```sql
CREATE TABLE IF NOT EXISTS meta (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS project (
    id         TEXT PRIMARY KEY,          -- url-safe slug, unique
    root       TEXT NOT NULL UNIQUE,      -- absolute path, canonicalised
    name       TEXT NOT NULL,             -- display name (directory name)
    last_seen  TEXT NOT NULL,             -- RFC-3339
    hidden     INTEGER NOT NULL DEFAULT 0 -- 1 = forgotten, cards preserved
);

CREATE TABLE IF NOT EXISTS card (
    id          TEXT PRIMARY KEY,         -- 12 lowercase alphanumerics
    project_id  TEXT NOT NULL REFERENCES project(id),
    title       TEXT NOT NULL,
    goal        TEXT NOT NULL DEFAULT '', -- prompt sent to `pilot run`; falls back to title
    notes       TEXT,
    labels      TEXT NOT NULL DEFAULT '', -- comma-separated, no spaces
    ord         REAL NOT NULL,            -- manual order within Backlog
    archived    INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_card_project ON card(project_id, archived);

CREATE TABLE IF NOT EXISTS dispatch (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    card_id     TEXT NOT NULL REFERENCES card(id) ON DELETE CASCADE,
    project_id  TEXT NOT NULL,
    run_id      TEXT NOT NULL,
    run_dir     TEXT NOT NULL,            -- absolute, for cross-project reads
    started_at  TEXT NOT NULL,
    ended_at    TEXT                      -- set when the run reaches a terminal status
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_dispatch_run ON dispatch(project_id, run_id);
CREATE INDEX IF NOT EXISTS idx_dispatch_card ON dispatch(card_id);

-- Derived cache. Safe to DELETE at any time; rebuilt on next read.
CREATE TABLE IF NOT EXISTS rollup (
    run_dir   TEXT PRIMARY KEY,
    mtime_ns  INTEGER NOT NULL,           -- state.json mtime when cached
    status    TEXT NOT NULL,              -- RunStatus, snake_case
    done      INTEGER NOT NULL,
    total     INTEGER NOT NULL,
    failed    INTEGER NOT NULL,
    blocked   INTEGER NOT NULL,
    review    INTEGER NOT NULL,
    usd       REAL NOT NULL,
    subrows   TEXT NOT NULL               -- JSON array of SubRow
);
```

### 3.2 Cache invalidation

Read path for one dispatch:

1. `state_mtime(run_dir)` -> `None` means the run directory is gone; mark the
   dispatch `ended_at` and treat the card as Backlog (see 4.3).
2. If a `rollup` row exists and its `mtime_ns` equals the current mtime, use it.
3. Otherwise `load_state(run_dir)`, compute the roll-up, upsert it.

Terminal runs therefore cost one `stat` per frame. `pilot resume` mutates a
terminal run's `state.json`, which changes the mtime, which invalidates the
cache — so resume is handled without a special case.

---

## 4. Types and derivation

### 4.1 Public types

```rust
pub struct Card {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub goal: String,
    pub notes: Option<String>,
    pub labels: Vec<String>,
    pub ord: f64,
    pub archived: bool,
    pub created_at: String,
    pub updated_at: String,
}

pub struct Rollup {
    pub status: RunStatus,
    pub done: usize,
    pub total: usize,
    pub failed: usize,
    pub blocked: usize,
    pub review: usize,
    pub usd: f64,
    pub subrows: Vec<SubRow>,
}

/// One planner task, flattened for display. Mirrors `dashboard::TaskRow`
/// plus the agent fields the card needs.
pub struct SubRow {
    pub task_id: String,
    pub title: String,
    pub status: TaskStatus,
    pub role: String,
    pub agent_name: Option<String>,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub usd: f64,
    pub attempts: u32,
    pub blocked_by: Vec<String>,   // unmet deps, computed (see 4.2)
    pub current_tool: Option<String>,
    // Detail-overlay fields. Projected straight off `Task`.
    pub deps: Vec<String>,         // every declared dep, met or not
    pub writes: usize,
    pub elapsed_secs: Option<i64>, // first in_progress -> now, or -> ended_at
    pub outcome: Option<String>,   // the worker's own summary
    pub worktree: Option<String>,
}

pub enum Column { Backlog, Planned, InProgress, Review, Done }

/// A card joined to its newest dispatch. What the renderer consumes.
pub struct BoardCard {
    pub card: Card,
    pub project_name: String,
    pub run_id: Option<String>,
    pub rollup: Option<Rollup>,
    pub column: Column,
    pub badges: Vec<Badge>,
}
```

### 4.2 `blocked_by`

Not stored on `Task`. Computed as: for a task whose status is `Pending` or
`Todo`, the subset of `task.deps` whose referenced task is not `Done`. Empty
for any task already `InProgress` or later. This is the one piece of scheduler
state the board reconstructs, and it is reconstructed from `RunState` alone —
no coupling to `scheduler.rs`.

### 4.3 `column_of` — normative

```rust
pub fn column_of(dispatch: Option<&Rollup>) -> Column {
    let Some(r) = dispatch else { return Column::Backlog };
    match r.status {
        RunStatus::Planning | RunStatus::AwaitingApproval => Column::Planned,
        RunStatus::Done | RunStatus::Failed | RunStatus::Aborted => Column::Done,
        RunStatus::Running | RunStatus::Merging => {
            // Every task that can still move is parked in review.
            let settled = r.done + r.review + r.failed;
            if r.review > 0 && settled >= r.total { Column::Review } else { Column::InProgress }
        }
    }
}
```

Rules this encodes, stated plainly:

- A card with no dispatch, or whose newest dispatch's run directory has been
  deleted, is **Backlog**. Deleting a run un-dispatches its card; it does not
  orphan it.
- A card whose newest run is terminal is **Done** regardless of how it ended.
  Re-running is an explicit `board dispatch`, which creates a *new* dispatch
  row and moves the card back to Planned.
- `blocked` never affects the column. A wholly blocked run still reads
  In Progress, with `x n blocked` on the card — that is accurate, and a
  Blocked column would hide the other six working tasks.

### 4.4 Badges

| Badge | Condition | Renders |
| --- | --- | --- |
| Progress | dispatched | `3/7` |
| Cost | `usd > 0.0` | `$1.24` |
| Failed | `failed > 0` | `!n failed` |
| Blocked | `blocked > 0` | `xn blocked` |
| Aborted | run `Aborted` | `aborted` |
| Attempts | any subrow `attempts > 1` | `retry xN` |
| Missing | run dir gone, or project root gone | `missing` |
| Labels | `labels` non-empty | first two, then `+n` |

---

## 5. CLI surface

New top-level `Board { #[command(subcommand)] action: Option<BoardAction> }`,
`display_order = 7` (next to the pilot family). No subcommand opens the TUI.

| Command | Behaviour |
| --- | --- |
| `wingman board` | Open the TUI on all non-hidden projects. |
| `wingman board add "<title>" [--goal G] [--project ID] [--label L]... [--notes N]` | Create a Backlog card. `--project` defaults to the registry entry for the cwd's project root. `--goal` defaults to the title. Prints the short card id. |
| `wingman board list [--project ID] [--column C] [--label L] [--all] [--json]` | Print cards with derived columns. `--all` includes archived. `--json` emits `Vec<BoardCard>`. |
| `wingman board show <CARD>` | One card: goal, notes, dispatch history, and the newest run's subrows. |
| `wingman board dispatch <CARD> [--model M] [--tier T] [--yes] [-- <pilot flags>]` | Spawn `wingman pilot run "<goal>" --detached` with the project root as cwd, record the dispatch, print the run id. Refuses if the card already has a live (non-terminal) dispatch unless `--again`. |
| `wingman board archive <CARD>` / `--restore` | Set/clear `archived`. Never deletes. |
| `wingman board rm <CARD>` | Hard delete, cascades dispatch rows. Requires `--yes` when the card has dispatches. |
| `wingman board projects [--forget ID] [--restore ID] [--relocate ID <PATH>]` | List/manage the registry. |
| `wingman board export [--json]` | Dump cards + dispatch history. The backup story for a per-machine DB. |

`<CARD>` accepts a unique id prefix (>= 4 chars), matching how run ids are
already resolved by `pick_run`. Ambiguous prefixes error with the candidates.

### 5.1 Dispatch mechanics

`board dispatch` resolves the card's project root, verifies it exists, then
spawns the current executable (`std::env::current_exe()`) with
`pilot run <goal> --detached` and `current_dir(project_root)`.

The run id is **minted by the board and passed in via `WINGMAN_RUN_ID`**, not
parsed out of the child's stdout: `pilot run` already honours that variable —
it is how a detached parent hands its id to the re-exec'd child — so the board
knows the id before the process starts and never depends on a print format. If
the child exits non-zero, no dispatch row is written and the error is surfaced
verbatim.

The decision half is `plan_dispatch(card, project, opts) -> DispatchPlan`,
which is pure: it picks the run id, builds the argv, and applies the refusals.
The spawn itself has no branching worth a stub binary.

Trailing `-- <pilot flags>` are forwarded unmodified, so every capability tier,
cost cap and sandbox flag `pilot run` already supports is reachable without the
board re-declaring them. `--worker-mode` is rejected (it is a pilot-internal
contract, same rule `serve` applies).

### 5.2 Auto-registration

A single entry point, called from the `pilot` and `board` command dispatchers
before anything else:

```rust
pub fn touch_project(root: &Path) -> Result<String, BoardError>;  // returns project id
```

Upserts by canonicalised `root`, refreshes `last_seen`, leaves `hidden`
untouched (so a forgotten project stays forgotten even if you keep working in
it), and returns the id. Slug derivation matches `ServeProject::effective_id`:
the directory name, lowercased, non-alphanumerics to `-`, with a `-2`, `-3`
suffix on collision.

On first open of `board.db`, every `[[serve.projects]]` entry is imported.

---

## 6. TUI

New module `crates/wingman-cli/src/commands/board_tui.rs`. It reuses
`Glyphs`, `LogView`, `SevFilter`, `HitAreas`, the confirm modal, the toast, and
the task detail overlay from `pilot_watch_tui.rs` — those move to a shared
`pilot_ui` module in Phase 4 rather than being duplicated.

### 6.1 Layout

Five fixed columns, horizontally scrollable when the terminal is narrow. Below
`100` columns the board degrades to a single-column list grouped by column
header, rather than rendering five unreadable slivers.

### 6.2 Keys

| Key | Action |
| --- | --- |
| `left`/`right`, `h`/`l` | Move between columns, skipping empty ones |
| `up`/`down`, `k`/`j` | Move between rows — cards and, when expanded, their tasks |
| `Enter` | On a card: expand/collapse. On a task: open its detail overlay. |
| `n` | New card: title, then goal (blank goal falls back to the title) |
| `e` / `g` | Edit the selected card's title / goal, seeded with the current value |
| `d` | Dispatch the selected card (confirm modal naming card and project) |
| `a` | Archive the selected card (restore is CLI-only) |
| `x` | Abort the selected card's live run (confirm modal) |
| `o` | Hand off to `pilot watch` for the newest run; re-enters on quit |
| `p` | Cycle project filter (all / one) |
| `/` | Fuzzy filter on title, label, project, agent name |
| `f` | Cycle badge filter: all / has-failed / has-blocked |
| `r` | Force reload, bypassing the roll-up cache |
| `?` | Help overlay |
| `q` / `Esc` / `Ctrl-C` | Quit, restoring the terminal |

Input precedence is innermost-first: an open prompt swallows every key except
`Esc`/`Enter`, then a confirm modal takes `y`/`n`/`Esc` only, then the detail
and help overlays close on any key. A `q` typed into the search box must
search, not quit; a `d` typed there must not dispatch.

`x` is the only board key that writes to a run, and it goes through
`control::append` — the same path `pilot watch` uses. Approve/veto/retry are
reachable via `o`, which hands off to the surface that already implements them.

`o` is a handoff, not an embed: the board tears its terminal down, calls
`pilot_watch_tui::run`, and re-enters when the user quits it. One TUI owns the
terminal at a time, and neither has to know about the other's state.

### 6.3 Refresh

Redraw every `250ms`; reload every 4th tick. Each reload `stat`s every live
dispatch's `state.json` and re-reads only those whose mtime changed.

**A reload never runs while a prompt, confirm modal or detail overlay is
open.** Otherwise a background refresh could move the selection out from under
a decision the user is in the middle of making. Selection also anchors to the
selected card's *id* across a reload, not its index, so a card appearing above
it does not shift the cursor.

### 6.4 Testability

Rendering must not require a terminal. `BoardView { columns: Vec<ColumnView> }`
with `to_ascii_width(usize) -> String`, mirroring `DashboardView::to_ascii`, so
layout is snapshot-tested headlessly. Only key handling and the ratatui draw
call live behind a terminal.

---

## 7. Failure modes

| Failure | Behaviour |
| --- | --- |
| `board.db` missing | Created empty. Registry re-populates on next `pilot`/`board` command. |
| `board.db` corrupt | Board refuses to start with the path and a pointer to `board export`; it never silently recreates over a corrupt file. Runs remain fully usable via `pilot watch`. |
| Project root moved or deleted | Card renders with a `missing` badge and is not dispatchable. `board projects --relocate` fixes it. |
| Run directory deleted | Dispatch is closed out (`ended_at` set); card returns to Backlog. |
| Two processes writing | WAL + `busy_timeout(5s)`. Registry upserts are single-statement; card writes are single-statement. No multi-statement transactions on the hot path. |
| `state.json` corrupt | `load_state` already errors; that dispatch renders `missing` and the rest of the board is unaffected. |
| Card dispatched twice | Guarded: refuses when a live dispatch exists unless `--again`. |

---

## 8. Test plan

Headless, no terminal, no provider.

**Model (Phase 1)**
- `AgentUsd` projects `model` onto `Agent`; empty string does not clobber.
- Old `state.json` without `model` deserialises to `None`.

**Store (Phase 2)**
- Open creates schema; reopen is idempotent; `schema_version` is `1`.
- `touch_project` upserts by root, is stable across calls, and does not clear `hidden`.
- Slug collision on two different roots with the same directory name yields `name` and `name-2`.
- `[[serve.projects]]` import runs once.

**Derivation (Phase 3)**
- `column_of` covers every `RunStatus`, plus the `Review` boundary
  (`review > 0 && done + review + failed >= total`).
- No dispatch -> Backlog. Terminal run -> Done for `Done`, `Failed`, `Aborted`.
- `blocked_by` lists only unmet deps, and is empty once a task is `InProgress`.
- Rollup cache: unchanged mtime hits cache; touched `state.json` misses; a
  resumed terminal run recomputes.

**CLI (Phase 3)**
- `add` -> `list --json` round-trips title, goal, labels, project.
- Card id prefix resolution: unique prefix resolves, ambiguous errors with candidates.
- `dispatch` against a stub executable records the run id and refuses a second
  live dispatch without `--again`.
- `archive` hides from default `list`, appears under `--all`.

**Render (Phase 4)**
- `BoardView::to_ascii_width` snapshots: empty board, one card per column,
  expanded card with subrows, narrow-terminal single-column fallback.
- Badge rendering for each row of the 4.4 table.

Target: every phase leaves `cargo fmt`, `cargo clippy --workspace
--all-targets -D warnings`, and the full suite green on Linux, macOS, Windows.
