# Board — implementation plan

Build order for `wingman board`. What it is: [BOARD.md](BOARD.md). Exact
schema, types and contracts: [BOARD-SPEC.md](BOARD-SPEC.md).

Six phases. Phases 1-3 are shippable and useful on their own — a scriptable
cross-project backlog with no TUI at all. The TUI is the last third of the
work, not the first.

> **Status: all six phases shipped.** 1068 tests green across the workspace
> (`wingman-board` 54, `wingman-cli` 217, `wingman-autonomous` 485),
> `cargo fmt --all --check` and `cargo clippy --workspace --all-targets
> -D warnings` clean. Deviations from this plan are recorded at the bottom.

---

## Decisions taken (and why)

| Decision | Alternative rejected | Why |
| --- | --- | --- |
| A card is a **goal**, expandable to planner tasks | One card per task (agtx) | `parse_plan` returns `Vec<PlannedTask>` — one goal fans out to N tasks. Card-per-task loses the goal the human actually wrote; card-per-goal-only shows less than `pilot watch` does today. Two levels keeps both. |
| Columns **derived**, never stored | A `column` field | A stored column is a second source of truth that drifts from `state.json` the moment a run advances. A pure function cannot disagree with `pilot watch`. |
| Global SQLite `~/.wingman/board.db` | Per-project `.wingman/board.jsonl` | A multi-project board cannot be backed by per-project files without an index anyway. `rusqlite` is already a dep (`wingman-learn`, `wingman-rag`) and `learn.db` sets the location precedent. Cost: the board does not travel with a clone — accepted, `board export` is the escape hatch. |
| Board **never writes** run state | Board owns a task table mirroring runs | One writer per fact. `RunStore` stays the sole writer of `tasks.jsonl`/`state.json`; the board holds only `run_id` and reads through the existing `dashboard` helpers. Removes reconciliation entirely. |
| Failed/Blocked are **badges** | Failed and Blocked columns | At goal level a run is mixed. One failed task out of seven must not drag a card out of In Progress while six others work. |
| New top-level `wingman board` | A `--board` flag on `pilot watch` | `pilot watch` is single-project by contract; the board is not. Reuses the watch TUI's internals, does not alter its behaviour. |
| Auto-register projects on use | `[[board.projects]]` config | Zero config, agtx-style. Paired with `--forget`/`--restore` so the registry has an eviction path — auto-register without one is how boards fill with dead repos. |
| Create-only, no drag between columns | Full drag-and-drop | The manager's scheduler owns task transitions; dep gating and write-set conflict detection are why runs converge. Abort/retry/approve/veto already have safe control-channel paths and stay reachable. |

---

## Phase 1 — `Agent.model` ✅ shipped

Standalone, lands before any board code.

**Files:** `crates/wingman-autonomous/src/model.rs`

- Add `pub model: Option<String>` to `Agent`, `#[serde(default)]`.
- Set it in `apply()` under the `Event::AgentUsd` arm when non-empty.
- Initialise `None` at every construction site.

**Why first:** "which model ran this" is the one card field with no backing
data — it exists only inside `agent.usd` events. Without the projection, every
frame would replay every event of every run to render one column.

**Tests:** projection from an event sequence; old snapshot deserialises to
`None`; empty model string does not clobber a set value.

**Effort:** ~20 lines. Independently reviewable, no board dependency.

---

## Phase 2 — store and registry ✅ shipped

**New crate:** `wingman-board`, depending on `wingman-autonomous` (read-only),
`wingman-config`, `rusqlite`, `serde`, `chrono`, `rand`. No ratatui, and no new
third-party crate — every one of those is already a workspace dependency.

- `store.rs` — `BoardStore::open_default()` -> `~/.wingman/board.db`, following
  `StatsStore::open`, plus `PRAGMA journal_mode=WAL`, `foreign_keys=ON`, and
  `busy_timeout(5s)`. Full DDL in spec §3.1.
- `registry.rs` — `touch_project(root) -> project_id`, slug derivation matching
  `ServeProject::effective_id`, collision suffixes, `forget`/`restore`/
  `relocate`, one-time `[[serve.projects]]` import.
- Call `touch_project` from the `pilot` and `board` command dispatchers.

**Ship value:** none visible yet. This is the only phase with no user-facing
surface; keep it small so it is reviewable as pure plumbing.

**Tests:** spec §8 "Store".

---

## Phase 3 — cards and CLI (no TUI) ✅ shipped

- `card.rs` — CRUD, prefix resolution (`>= 4` chars, matching `pick_run`).
- `rollup.rs` — `state_mtime` gate, `load_state`, roll-up computation,
  `blocked_by`, `rollup` table upsert.
- `column.rs` — `column_of` and badge derivation (spec §4.3, §4.4).
- `dispatch.rs` — spawn `current_exe() pilot run "<goal>" --detached` with the
  project root as cwd, passing the run id in via `WINGMAN_RUN_ID`, insert the
  dispatch row, forward trailing pilot flags, reject `--worker-mode`.
- CLI: `add`, `list`, `show`, `dispatch`, `archive`, `rm`, `projects`,
  `export` (spec §5).

**Ship value:** real. A scriptable multi-project backlog:
`wingman board add`, `wingman board dispatch`, `wingman board list --json`.
Usable in scripts and cron before a single pixel of TUI exists.

**Tests:** spec §8 "Derivation" and "CLI". Dispatch is split into a pure
`plan_dispatch` (ids, argv, refusals) and a six-line `spawn`, so the decisions
are tested without a stub binary and no provider key or real run is needed.

---

## Phase 4 — read-only column renderer ✅ shipped

- Extract `Glyphs`, `LogView`, `SevFilter`, `HitAreas`, the confirm modal, the
  toast and the task detail overlay out of `pilot_watch_tui.rs` into a shared
  `pilot_ui` module. **Pure move, no behaviour change** — `pilot watch` must
  render byte-identically before and after, and its existing tests are the
  proof.
- `wingman-board::view` — `BoardView { columns: Vec<ColumnView> }` with
  `to_ascii_width(usize)`, mirroring `DashboardView::to_ascii`, so layout is
  snapshot-tested with no terminal.
- `crates/wingman-cli/src/commands/board_tui.rs` — draw `BoardView`, five
  columns, single-column fallback under 100 cols, navigation keys only
  (`arrows`, `p`, `/`, `f`, `r`, `?`, `q`), 250ms mtime-gated poll.

**Ship value:** the board you can look at.

**Risk to watch:** the extraction in step 1 is the only change in this whole
plan that can regress a shipped surface. Do it as its own commit, ahead of the
board renderer, so a bisect lands on it cleanly.

---

## Phase 5 — expandable cards ✅ shipped

- ✅ `Enter` expands a card into its subrows: status glyph, agent name, model,
  cost, `dep T2` when the scheduler is holding it. *(landed in Phase 4 — the
  headless renderer already handled sub-rows, so wiring the key cost one
  handler.)*
- ✅ Sub-rows became first-class navigable rows (`Entry::Card` / `Entry::Sub`),
  so `Enter` on one opens its task detail; `session_id` names the worker
  transcript.
- ✅ `o` hands off to `pilot watch` for the card's newest run and re-enters the
  board when you quit it.

**Ship value:** the two-level board described in BOARD.md. This is the phase
that makes "which agent, which model, what logs" answerable without leaving
the board.

---

## Phase 6 — create and dispatch from the board ✅ shipped

- ✅ `n` — two-stage new-card prompt (title, then optional goal).
- ✅ `e` / `g` — edit title / goal, seeded with the current value.
- ✅ `d` — dispatch behind a confirm modal naming the card and project. A card
  never spends money without it.
- ✅ `a` — archive. Restore stayed on the CLI (`board archive <id> --restore`):
  an archived card has left the board, so there is nothing to select.
- ✅ `x` — abort the live run via `control::append`, the same path `pilot watch`
  uses. The only board key that writes to a run, and it confirms first.

**Ship value:** feature-complete against the agreed scope.

---

## Docs, last

- `docs/CLI.md` — the `board` subcommand table.
- `docs/PILOT-MODE.md` — a pointer from pilot to the board.
- `docs/INDEX.md` — entry under "Pilot Mode & Roadmap" (added with this plan).
- ✅ `docs/CLI.md` — the `board` subcommand table.
- ✅ `docs/INDEX.md` — entry under "Pilot Mode & Roadmap".
- ✅ `README.md` — a "The board" section and a docs-table row.
- ✅ BOARD.md's status line flipped to **shipped**; deviations recorded below.
- ✅ `docs/PILOT-MODE.md` — a pointer from pilot to the board.

---

## Deviations from this plan

Recorded as they happened, following the precedent in
[HTTP-API-PLAN.md](HTTP-API-PLAN.md).

**No `uuid` dependency.** The spec said card ids were uuid v4. The workspace
has no `uuid` crate and pilot mints its own ids with `rand`, which *is* a
workspace dependency. Card ids are now 12 lowercase alphanumerics from the same
generator. One fewer dependency for an id nobody parses.

**Dispatch does not scrape stdout.** The spec had `board dispatch` parse the
run id out of `pilot run --detached`'s output. It turned out `pilot run`
already honours `WINGMAN_RUN_ID` — that is how a detached parent hands its id
to the re-exec'd child — so the board mints the id up front and knows it before
the process starts. More robust, and it reuses an existing contract instead of
depending on a print statement's format.

**The Phase 4 extraction was smaller than planned.** The plan named `Glyphs`,
`LogView`, `SevFilter`, `HitAreas`, the confirm modal, the toast and the detail
overlay. Only `Glyphs`, `setup`/`teardown`, the `Term` alias and a `centered`
helper actually have two callers today — the board has no log pane, so
`LogView`/`SevFilter` would have been moved code with one caller. They stay in
`pilot_watch_tui.rs` until phases 5-6 need them. `pilot_ui.rs` is a pure move
and the watch TUI's existing snapshot tests pass unchanged.

**Expand/collapse landed in Phase 4, not 5.** The headless renderer already
handled sub-rows, so wiring `Enter` cost one key handler. Shipping a board
where `Enter` does nothing would have been worse. The rest of Phase 5 — the
shared task detail overlay and the `o` handoff to `pilot watch` — is still open.

**`board list --json` gained a `--label` and `--all` path** not in the spec's
command table, and the empty-result message distinguishes "no cards" from "no
cards match that filter". Both fell out of using the thing.

**The board renders its own task detail rather than sharing the watch TUI's.**
The plan assumed one overlay for both. `pilot watch`'s is built on
`DashboardModel`'s `TaskRow`/`AgentRow`; the board's is built on `SubRow`,
which carries a field the watch shape has no slot for — the model that ran the
task. Generalising one overlay across two data shapes would have been more
code than the ~50 lines the board's own costs, and it would have meant editing
a shipped surface for the board's benefit. `SubRow` grew `deps`, `writes`,
`elapsed_secs`, `outcome` and `worktree` to feed it, and `board show` prints
the same facts headlessly.

**`a` archives but does not restore.** An archived card is gone from the board,
so there is no row left to press a key on. Restore lives on the CLI.

**Sub-rows needed a flat entry model.** Card-level `(col, row)` indexing could
not address a task, so selection became `Vec<Entry>` per column with
`Entry::Card(i)` / `Entry::Sub(i, j)`. That also fixed a latent papercut: the
cursor now anchors to the selected card's id across a reload instead of
holding a positional index that a background refresh could shift.

**Three bugs found by using it, all fixed:** Windows `canonicalize` returns
`\?\`-prefixed paths that leaked into every path we printed (stripped once,
at the point paths enter the store), and derived `Debug` ignores width
specifiers so `{:<11?}` silently refused to pad the status column in
`board show`. The third: `run_loop` reloaded every 4th tick regardless of
modal state, so a background refresh could move the selection out from under
an open confirm dialog — reloads are now suppressed while a prompt, confirm or
detail overlay is open.

---

## Risks

**The shared-UI extraction (Phase 4).** `pilot_watch_tui.rs` is 2,276 lines and
shipped across PRs #10-#14. Moving pieces out of it is the one step that can
break something users already rely on. Mitigation: pure move in its own commit,
existing watch tests unchanged and green.

**Cross-project read cost.** Ten projects x forty runs is 400 `stat` calls per
tick if done naively. Mitigation: only live dispatches are stat'd per tick;
terminal runs come from the `rollup` cache; card and registry reads are
mutation-driven, not per-tick. If this still bites, the fallback is to widen
the poll interval, not to add a daemon.

**Per-machine backlog.** `~/.wingman/board.db` does not travel with a clone and
is not shared with a team. This is a deliberate consequence of the storage
choice. `board export --json` exists so the data is never trapped; if team
sharing is ever wanted, the honest answer is a `serve`-backed board, not
syncing a SQLite file.

**Registry drift.** Auto-registration means the board sees every repo you ever
ran pilot in. `--forget` is sticky by design (working in a forgotten repo does
not un-forget it), which is the right default but will surprise someone once.
Documented in BOARD.md.

**Scope creep toward drag-and-drop.** The first request after Phase 6 will be
"let me drag a card". Forcing a task transition means overriding dep gates and
the write-set conflict check, which is the machinery that makes runs converge.
If it is ever built, it belongs in the orchestrator with its own gate, not in
the renderer.
