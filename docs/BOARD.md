# Board (`wingman board`)

A persistent, multi-project kanban board over pilot runs. Cards are goals you
author; they outlive the runs that execute them. Columns are derived from run
state, never stored.

```bash
wingman board                        # the TUI
wingman board add "Fix LSP restart storm"
wingman board dispatch c3f1          # starts a pilot run for that card
wingman board list --json            # scriptable
```

Status: **shipped.** Build order and the reasoning behind each decision are in
[BOARD-PLAN.md](BOARD-PLAN.md); exact schema, types, and derivation rules are
in [BOARD-SPEC.md](BOARD-SPEC.md).

---

## Design in one paragraph

Wingman already has the card. `wingman-autonomous`'s `Task` carries status,
agent, worktree, cost, commits, attempts, timings and an outcome; `Agent`
carries a friendly name, role, pid, live tool, and the `session_id` of its own
transcript. `dashboard.rs` already projects those into `TaskRow`/`AgentRow`/
`LogRow`, and `pilot_watch_tui.rs` already renders them live with a detail
overlay and a control channel. What does *not* exist is durable identity: a
pilot task is born when the planner plans it and dies with the run. So the
board adds exactly one thing — a **card** in a global SQLite store that
survives runs and spans projects — and derives everything else by reading run
state off disk. One writer per fact, no reconciliation.

---

## Two levels, because a goal fans out

agtx's card is one agent session. Wingman's unit of human intent is a *goal*,
and `planner::parse_plan` returns `Vec<PlannedTask>` — one goal becomes N tasks
in one run. Flattening that into one card per task loses the goal; flattening
it into one card per goal loses the plan. So the board shows both:

```
+- BACKLOG ------+- PLANNED ------+- IN PROGRESS --+- REVIEW -------+- DONE ---------+
| > wingman      | > wingman      | v wingman      | > agtx         | > wingman      |
|   Fix LSP      |   Add board    |   Refactor RAG |   Bench runner |   Windows job  |
|   --           |   7 tasks      |   3/7 - $1.24  |   6/6 - $0.88  |   4/4 - $2.10  |
|                |                |  |- T1 impl  o |                |                |
|                |                |  |  brave_otter|                |                |
|                |                |  |  opus-5 $0.4|                |                |
|                |                |  |- T2 tests * |                |                |
|                |                |  |  calm_finch |                |                |
|                |                |  `- T3 docs  x |                |                |
|                |                |       dep T2   |                |                |
+----------------+----------------+----------------+----------------+----------------+
```

- **Card row** — durable. Project, title, roll-up (`3/7 · $1.24`), badges.
- **Sub-row** — ephemeral, projected live from the run's `state.json`. One per
  planner task: status glyph, agent name, model, cost, and `dep T2` when the
  scheduler is holding it. Sub-rows are navigable; `Enter` on one opens its
  task detail — deps, writes, attempts, elapsed, spend, worker, model,
  worktree, the worker's outcome, and the session id of its transcript.

A card is 1:N with *runs* over time too — a failed run leaves the card in Done
with a failure badge, and `board dispatch` again starts a fresh run linked to
the same card. That history is what makes "this goal took three attempts and
$6.40" answerable.

---

## Storage — three layers, one writer each

| Layer | Home | Owner | Why there |
| --- | --- | --- | --- |
| Card identity, project registry, dispatch history | `~/.wingman/board.db` | board (new) | Must outlive runs and span projects. A per-project file cannot back a multi-project board. |
| Execution truth — tasks, agents, cost, events | `<project>/.wingman/autonomous/<run>/tasks.jsonl` + `state.json` | `RunStore` (unchanged) | Already append-only and already the source of truth. The board never writes here. |
| Roll-up cache | `board.db` table `rollup`, keyed by run dir + `state.json` mtime | board (derived) | Stops a 40-run history from being replayed on every frame. Deleting the table is always safe. |

`rusqlite` (bundled) is already a dependency of `wingman-learn` and
`wingman-rag`, and `~/.wingman/learn.db` sets the precedent for a global DB.
The board adds **no new crate**.

The board holds a card's `run_id`; everything else about that run is read
through `dashboard::list_runs` / `load_state` / `state_mtime`, which already
exist. If `board.db` is deleted you lose your backlog, not your work — every
dispatched run is still on disk and still visible in `pilot watch`.

---

## Columns are derived, not stored

There is no `column` field. A card's column is a pure function of its dispatch
state and the run's roll-up:

| Column | Condition |
| --- | --- |
| Backlog | no live dispatch |
| Planned | run status `Planning` or `AwaitingApproval` |
| In Progress | run `Running`/`Merging`, and some task not yet `Review`/`Done` |
| Review | run `Running`/`Merging`, and every non-failed task is `Review` or `Done` |
| Done | run `Done`, `Failed`, or `Aborted` |

`Failed` and `Blocked` are **badges, not columns**. At goal level a run is
usually mixed — one failed task out of seven should not drag the card into a
Failed column while six others are still working. They surface as `!1 failed` /
`x2 blocked` on the card, as glyphs on sub-rows, and as a filter key.

Because the function is pure, the board never disagrees with `pilot watch`.
Both read the same `state.json`.

---

## The project registry

`wingman board` is the only multi-project surface in the CLI — every other
pilot command resolves one project root. The registry is populated
**automatically**: any `pilot` or `board` command upserts
`(root, name, last_seen)` into `board.db`. No config to write, no repo to
remember to add.

Auto-registration without an eviction path is how a board fills with dead
repos, so `wingman board projects --forget <id>` hides one (cards preserved)
and `--restore` brings it back. `[[serve.projects]]` entries are imported on
first open so `serve` users start populated.

Cross-project reads are mtime-gated: only non-terminal runs are re-read each
tick; terminal runs come from the `rollup` cache.

---

## Keys

| Key | Does |
| --- | --- |
| arrows / `hjkl` | Move between columns and rows |
| `Enter` | Expand a card, or open a task's detail |
| `n` | New card — title, then goal |
| `e` / `g` | Edit the card's title / goal |
| `d` | Dispatch: start a pilot run **(confirms)** |
| `a` | Archive the card |
| `x` | Abort the card's live run **(confirms)** |
| `o` | Open the run in `pilot watch`, returning here on quit |
| `p` | Cycle the project filter |
| `/` | Search title, project, label, agent |
| `f` | Cycle badge filter: all / has-failed / has-blocked |
| `r` | Force reload, bypassing the roll-up cache |
| `?` | Help |
| `q` / `Esc` | Quit |

Two keys write, and both confirm first: `d` spends money, `x` cancels
in-flight workers. Everything else is either a card edit or a view change.
Card edits go to `board.db`; `x` is the board's only write to a run, and it
goes through `control.jsonl` — the same channel `pilot watch` uses.

`o` is a real handoff, not an embed: the board tears its terminal down, hands
it to `pilot watch`, and re-enters when you quit that. One TUI owns the
terminal at a time.

---

## What it is not

- **Not a drag-and-drop board.** You create and dispatch cards; you do not
  force a task from Todo to InProgress. The manager's scheduler owns task
  transitions, and dep gating plus write-set conflict detection are the reason
  pilot runs converge. The board exposes the transitions that already have
  safe control-channel paths — abort, retry, approve, veto — via
  `control.jsonl`, exactly as `pilot watch` does.
- **Not a replacement for `pilot watch`.** `pilot watch` stays single-project
  and unchanged; it is the right tool when you are watching one run closely.
  The board is the cross-project, cross-run view.
- **Not a web surface.** TUI only. `serve` already exposes `RunState` as JSON
  and SSE, so a web board is cheap later, but it is out of scope here.
- **Not shared with your team.** `~/.wingman/board.db` is per-machine and does
  not travel with a clone. `board export --json` is the escape hatch.

---

## Prerequisite: `Agent.model` (shipped)

The model that ran a task used to be unqueryable: it appeared only inside
`Event::AgentUsd { model, .. }` in the event stream and was never projected
onto `Agent` by `apply()`. Rendering "which model did this" would have meant
replaying every event, on every frame, for every run.

`Agent.model: Option<String>` now carries it, set from `agent.usd` in
`apply()`. Last write wins, and an empty model string never clobbers a name
already recorded — providers that don't report one would otherwise erase it.
`#[serde(default)]` means older `state.json` snapshots load unchanged, and
because `tasks.jsonl` already carried `model` on every `agent.usd` line,
replaying an old run backfills the field for free.

---

## See also

- [PILOT-MODE.md](PILOT-MODE.md) — the shipped `wingman pilot` surface.
- [AUTONOMOUS-MODE.md](AUTONOMOUS-MODE.md) — the data model the board reads.
- [WATCH-UX-ENHANCEMENTS.md](WATCH-UX-ENHANCEMENTS.md) — the watch TUI internals
  the board renderer reuses.
- [HTTP-API.md](HTTP-API.md) — `RunState` over JSON/SSE, if a web board follows.
