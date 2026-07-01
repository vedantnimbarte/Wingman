# Autonomous Mode (M8 - Planned)

This document describes the planned autonomous mode feature for Arc-Code. As of now, this is a **design specification** in the planning phase. See `/plan.md` in the repository root for the full implementation roadmap.

## Vision

`arccode autonomous "<goal>"` will allow you to describe a multi-task piece of work in natural language, and the system will:

1. **Plan** the work into discrete, parallel-friendly tasks with dependencies.
2. **Spawn** a manager agent that orchestrates worker agents.
3. **Delegate** each task to a specialized worker (developer, designer, tester, reviewer) in an isolated git worktree.
4. **Converge** their work into a single integration branch.
5. **Open a PR** for review and merge.

All work happens locally. No cloud upload. Cross-platform (Windows and Unix from day 1).

## User-Facing Examples

### Basic Invocation

```bash
$ arccode autonomous "add dark-mode toggle to the TUI"

[autonomous] planning…
[autonomous] proposed 7 tasks (run id: 2026-05-27-1430-a3f):
  1. [developer] Add `theme.mode` field to tui config (deps: —)
  2. [developer] Wire toggle key (`Ctrl+T`) into composer
  3. [designer]  Define dark palette in arccode-tui::theme
  4. [developer] Update welcome screen for dark mode
  5. [tester]    Write integration test for dark-mode toggle
  6. [tester]    Manual testing checklist
  7. [reviewer]  Final review + changelog entry

Approve plan? [y / e (edit) / n] y

[autonomous] spawning manager…
[autonomous] manager → developer #1  worktree=auto-2026-05-27-1430-a3f-task-1
[autonomous] manager → designer  #3  worktree=auto-2026-05-27-1430-a3f-task-3
[autonomous] task 1 done (developer, 2m18s, $0.07)
[autonomous] task 3 done (designer,  3m02s, $0.11)
…
[autonomous] all tasks done. merging worktrees into arccode/auto/2026-05-27-1430-a3f…
[autonomous] PR opened: https://github.com/vedantnimbarte/Arc-Code/pull/42
```

### Plan-Only Mode

```bash
$ arccode autonomous --plan-only "refactor error handling in arccode-core"

[autonomous] planning…
[autonomous] wrote tasks.jsonl (4 tasks, 0 dependencies)
```

Useful for review before committing to the work.

### Resume an Interrupted Run

```bash
$ arccode autonomous --resume 2026-05-27-1430-a3f

[autonomous] resuming run 2026-05-27-1430-a3f (3/7 tasks done)
[autonomous] task #4 status was in_progress; restarting…
…
```

## Data Model

### Task Representation

Each task in the plan is a discrete unit of work:

```json
{
  "id": "task-1",
  "role": "developer",
  "title": "Add theme.mode field to tui config",
  "goal": "Introduce a configuration option to track dark/light mode preference",
  "acceptance": "theme.mode is readable in config, defaults to 'light', user can toggle",
  "deps": []
}
```

### Task Lifecycle

```
pending (created, deps not met)
    ↓
todo (deps met, awaiting agent)
    ↓
in_progress (agent working)
    ↓
review (agent reported complete, awaiting integration)
    ↓
done (merged into integration branch)
    or
failed (agent failed)
    or
blocked (merge conflict or merge error)
```

### Run Storage

Each autonomous run creates:

```
<project>/.arccode/autonomous/<run-id>/
├── tasks.jsonl              # append-only event log
├── state.json               # latest state snapshot (atomic writes)
├── worktrees/
│   ├── auto-...-task-1/     # developer's worktree
│   ├── auto-...-task-3/     # designer's worktree
│   └── …
└── sessions/
    ├── manager-<session-id>.jsonl
    ├── worker-<agent-id>.jsonl
    └── …
```

**tasks.jsonl** (append-only events):

```jsonc
{"t":"2026-05-27T14:30:01Z","ev":"task.create","id":"t1","role":"developer","title":"…","deps":[]}
{"t":"…","ev":"task.status","id":"t1","status":"todo"}
{"t":"…","ev":"task.assign","id":"t1","agent":"agent-7f3a","worktree":"auto-…-t1"}
{"t":"…","ev":"task.status","id":"t1","status":"in_progress"}
{"t":"…","ev":"task.tool","id":"t1","agent":"agent-7f3a","tool":"edit_file","ok":true}
{"t":"…","ev":"task.status","id":"t1","status":"review","outcome":{"summary":"…","files_changed":4}}
{"t":"…","ev":"agent.usd","agent":"agent-7f3a","usd":0.07}
{"t":"…","ev":"task.status","id":"t1","status":"done"}
{"t":"…","ev":"run.merge.task","id":"t1","strategy":"squash","commit":"abc123"}
{"t":"…","ev":"run.pr","url":"https://…/pull/42"}
{"t":"…","ev":"run.done"}
```

**state.json** (latest snapshot, written atomically after each event):

```json
{
  "run_id": "2026-05-27-1430-a3f",
  "goal": "add dark-mode toggle to the TUI",
  "base_commit": "346077d…",
  "integration_branch": "arccode/auto/2026-05-27-1430-a3f",
  "status": "running",
  "tasks": [
    {"id":"t1","role":"developer","title":"…","status":"done","deps":[],"agent":"agent-7f3a","worktree":"…","usd":0.07,"commits":["abc123"]},
    {"id":"t2","role":"developer","title":"…","status":"in_progress","deps":["t1"],"agent":"agent-9c1b","worktree":"…","usd":0.03},
    …
  ],
  "agents": [
    {"id":"agent-7f3a","role":"developer","current_task":"done","pid":null,"status":"idle"},
    {"id":"agent-9c1b","role":"developer","current_task":"t2","pid":12345,"status":"in_progress"},
    …
  ],
  "totals": {"usd": 0.42, "tokens_in": 12345, "tokens_out": 4567}
}
```

## Architecture (Planned)

### Crate Structure

New crate: `arccode-autonomous`

```
crates/arccode-autonomous/
├── Cargo.toml
└── src/
    ├── lib.rs              # public Orchestrator API
    ├── orchestrator.rs     # run lifecycle, spawning, merge, PR
    ├── planner.rs          # initial planning call to manager
    ├── manager.rs          # manager agent loop + tool registry
    ├── worker.rs           # subprocess supervisor + event parser
    ├── store.rs            # RunStore: tasks.jsonl + state.json persistence
    ├── model.rs            # Task, Agent, Run, Status, Role
    ├── worktree.rs         # create / cleanup / merge helpers
    ├── pr.rs               # gh integration (with fallback)
    ├── role.rs             # AgentRole loader (~/.arccode/agents/)
    └── tools/              # manager-only tools
        ├── mod.rs
        ├── add_task.rs
        ├── assign_task.rs
        ├── reassign_task.rs
        ├── finalize_task.rs
        ├── message_agent.rs
        └── abort_task.rs
```

### Agent Roles

Predefined roles with system prompts:

| Role        | Responsibilities                                              |
|-------------|---------------------------------------------------------------|
| `developer` | Code changes, implementation, bugfixes.                        |
| `designer`  | UX/visual design, theme palettes, layout.                     |
| `tester`    | Test writing, test execution, quality assurance.              |
| `reviewer`  | Code review, changelog, PR description, final QA.             |

Custom roles can be added at `~/.arccode/agents/<role>.md` with a user-defined system prompt.

### Process Model

```
User invokes: arccode autonomous "<goal>"
    ↓
[Planner] call manager model with planning prompt
    ↓
Manager returns: structured task list (JSON)
    ↓
[Orchestrator] render plan, prompt user (y/e/n)
    ↓ (if approved)
[Orchestrator] spawn in-process manager agent loop
    ↓
Manager loop runs continuously:
  1. Scan state.json for eligible tasks (deps met, under concurrency cap)
  2. For each: call assign_task tool → Orchestrator spawns worker
  3. Worker runs as subprocess: `arccode --print --json --worker-mode --task-file <path>`
  4. Manager receives tool outcomes, calls finalize_task when worker done
  5. Exit when all tasks done or error
    ↓
[Orchestrator] worktrees are merged into integration branch (squash commits)
    ↓
[PR] gh pr create (or push + print URL if gh unavailable)
    ↓
Run done, integration branch left behind for user inspection
```

## CLI & Configuration

### Subcommand

```bash
arccode autonomous <GOAL> [OPTIONS]

OPTIONS:
  --plan-only              Plan and write tasks.jsonl, don't spawn workers
  --resume <RUN_ID>        Resume an interrupted run
  --max-agents <N>         Override [autonomous].max_concurrent_agents
  --max-usd <FLOAT>        Override [autonomous].max_usd cap
  --no-pr                  Skip gh pr create; just push the branch
  --yes                    Auto-approve the plan (no interactive gate)
  --base <REV>             Branch from <REV> instead of HEAD
```

### Configuration Section

In `config.toml`:

```toml
[autonomous]
# Model for the manager agent (defaults to router.default_model)
manager_model = "anthropic/claude-opus-4-7"

# Model for worker agents (defaults to router.fast_model)
worker_model = "anthropic/claude-haiku-4-5-20251001"

# Concurrency cap
max_concurrent_agents = 4

# Cost cap (USD); abort run if exceeded
max_usd = 10.0

# Per-task timeout (seconds)
task_timeout_secs = 1800

# Worktree base directory (relative to project root)
worktree_base = ".arccode/worktrees"

# Integration branch prefix
integration_branch_prefix = "arccode/auto"

# Custom role definitions (per role.md file)
# Defaults to ~/.arccode/agents/
agents_dir = "~/.arccode/agents"

# gh path (for PR creation fallback)
gh_path = "gh"
```

## TUI Integration (Planned)

When a run is active, the TUI shows:

- **Top bar indicator:** `Autonomous: <run-id> · 3/7 done`
- **Ctrl+A dashboard:** Split-pane view showing tasks, agents, live log

### Dashboard Layout

`arccode pilot watch` renders a live, colour-coded, 2-column grid: **Tasks |
Agents** on the top row, with the **Live log** spanning the full width below.
The header carries the run summary (progress by status, elapsed, spend, and
the git anchors). Each row surfaces the details worth watching — per-task cost,
elapsed time, write-set size, dependency list, and retry count; per-agent
current tool, uptime, pid, and cost.

Workers are shown by a Docker-style **friendly name** (`brave_otter`,
`lucid_lynx`) rather than the raw `agent-000N` id. The name is derived
deterministically from `(run_id, agent id)` — so replaying a run yields the
same names — and in the interactive UI each worker's name gets a stable colour
so it's easy to track across the live log. The stable `agent-000N` id is kept
internally (session files, `session fork`, event log) and still appears in
`state.json` alongside the name.

```
┌ Pilot: 2026-07-01-0707-hq27zr · 3/16 · Running · $0.42 · 2▶ · 1✗ · 4m12s ──────┐
┌─ Tasks (16) ────────────────────────┐┌─ Agents (4) ─────────────────────────┐
│ ✓ t1  [designer]  Tiptap editor …   ││ ↻ brave_otter [designer] task=t1 ·   │
│ ↻ t2  [developer] Editor.tsx  ·      ││     ▸edit_file · pid=10628 · 3m · $.08│
│       lucid_lynx · deps: t1 · ✎3 ·   ││ ↻ lucid_lynx  [developer] task=t2 ·  │
│       $0.05 · 1m20s · try2           ││     ▸run_shell · pid=19572 · 2m · $.05│
│ ○ t3  [developer] Slash menu …       │└──────────────────────────────────────┘
└──────────────────────────────────────┘
┌─ Live log ─────────────────────────────────────────────────────────────────────┐
│ 07:24:44  task.tool    t2 [lucid_lynx] edit_file                               │
│ 07:25:07  task.status  t1e → Failed                                            │
│ 07:25:53  task.create  t1f: Create editor package …                            │
└────────────────────────────────────────────────────────────────────────────────┘
```

In-progress tasks and the workers running them show an animated circular
spinner (`◐◓◑◒`) in place of the static status glyph, so the work happening
*right now* is obvious at a glance. The spinner is driven off wall-clock time,
so it rotates smoothly regardless of the `--interval-ms` state-poll cadence.

Under the header is a **meters** row: a progress gauge (done/total) whose label
carries the percent, a linear **ETA** (from the average time per completed
task) and the **spend-rate** (`$/min`), next to a **spend sparkline** that
plots cumulative cost over the run. ETA and rate appear once there's enough
signal to estimate them.

Terminals that can't render the unicode glyphs (legacy Windows console,
non-UTF-8 locales) are auto-detected and fall back to a plain-ASCII glyph set;
pass `--ascii` to force it, or set `ARCCODE_ASCII=0` to force unicode.

**Multiple runs.** When more than one run is active, a **Runs sidebar** appears
on the left of the top row (Runs | Tasks | Agents) listing each active run with
its status glyph and progress; the watched run is marked `▸`. `Tab` moves focus
between the Runs and Tasks panes (the focused pane gets a cyan border), `↑`/`↓`
then select a run or scroll tasks, and number keys `1`–`9` jump straight to a
run. The sidebar refreshes ~once a second as runs start and finish; the run
you're watching stays listed even if it completes. With a single active run the
sidebar is hidden and the layout is the plain Tasks | Agents grid.

```
┌ Pilot: 2026-07-01-0707-hq27zr · 3/16 · Running ────────────────────────────────┐
┌ Runs (2) ──┐┌ Tasks (16) ───────────┐┌ Agents (2) ──────────────────────────┐
│▸◐ hq27zr 3/16││ ↻ t2 [dev] Editor…    ││ ↻ brave_otter [dev] task=t2 · ▸edit… │
│ ◐ 9f4d2  1/5 ││ ○ t3 [dev] Slash…     ││ ↻ lucid_lynx  [dev] task=t3 · ▸run…  │
└────────────┘└───────────────────────┘└──────────────────────────────────────┘
┌ Live log ──────────────────────────────────────────────────────────────────────┐
│ 07:24:44  task.tool    t2 [brave_otter] edit_file                              │
└────────────────────────────────────────────────────────────────────────────────┘
```

**Interaction.** `Tab` cycles focus through the visible panes (Tasks → Live log
→ Runs); the focused pane gets a cyan border. The arrows (`↑`/`↓` or `k`/`j`),
`PgUp`/`PgDn`, and `Home`/`End` (`g`/`G`) drive the focused pane — moving the
run selection, the highlighted task, or the log scroll position.

- **Tasks** — the highlighted row is a selection; `Enter` opens a **detail
  overlay** with the full title, deps, write-set, attempts, spend, and the
  assigned worker's tool/pid/uptime, plus that task's recent log lines.
- **Live log** — no longer force-stuck to the bottom: scroll up to read back
  (`End`/`G` re-attaches follow-mode). `/` opens a case-insensitive search
  (matches agent names, task ids, tool names); `f` cycles the severity filter
  (all → warn+ → errors). The pane title shows the active filter and a
  `(paused)` marker when detached.
- **Runs** — `1`–`9` jump straight to the first nine runs; beyond that, focus
  the Runs pane (`Tab`) and use the arrows or `Home`/`End`.
- **Mouse** — the wheel scrolls whichever pane is under the pointer; left-click
  focuses a pane and selects the clicked run or task row.
- `?` toggles a keybinding overlay; `q`/`Esc`/`Ctrl-C` exits.

The terminal **bell** rings once on a new task failure and once when the
watched run finishes (mute with `ARCCODE_NO_BELL`).

When stdout is not a terminal (CI, `| tee`, redirected logs), `pilot watch`
falls back to a plain reprint of the current run's grid (no sidebar, no
animation, no meters); `pilot status` prints that grid once and exits.

### Controlling a live run

`pilot watch` and `pilot status` observe; the **control channel** lets you
steer a run from a different process than the one hosting the orchestrator.
Commands are newline-delimited JSON appended to `<run-dir>/control.jsonl`; the
orchestrator's control watchdog tails the file and applies each command once.

From the CLI:

```
pilot abort [RUN]            # abort the whole run
pilot abort [RUN] --task T   # abort just task T's worker
pilot retry T [RUN]          # re-queue a failed/blocked task
pilot approve [RUN]          # release a plan-approval gate
pilot veto [RUN]             # reject a pending plan
```

`RUN` defaults to the most recently updated run. In a pinch you can write the
file by hand: `echo '{"cmd":"abort_run"}' >> <run-dir>/control.jsonl`.

From the **watch UI**: `x` aborts the run (with a confirm), `r` retries the
selected task (with a confirm), and — while a run is parked at the plan gate —
`a` / `v` approve / veto it. `abort_run` cancels every in-flight worker, marks
the remaining tasks failed, records `RunStatus::Aborted`, and refuses further
assignment so the run winds down cleanly.

Approve/veto surface the run as `AwaitingApproval` in the dashboard and are
honoured by the **notify-only** approval window. The **hard** gate normally
requires an interactive TTY, but a headless hard-gate run started with
`pilot run --await-approval [--approval-timeout SECS]` will instead park at the
gate and wait for a control-channel `approve` / `veto` (from another terminal
or the watch UI). Unlike the notify window, the hard gate **denies by default**:
if the window (600s by default) elapses with no decision, the plan is rejected,
so unattended CI fails closed rather than proceeding unsupervised.

### Slash Commands

- `/autonomous status` — print run summary.
- `/autonomous abort` — terminate manager and workers; leave worktrees.
- `/autonomous resume` — re-attach to a running run.

## Limitations & Constraints (Planned v1)

- **Depth = 1:** Workers cannot spawn subagents or nested autonomous runs.
- **Bash-only merge:** Merge strategy is squash + auto-linearize by task deps. Conflicts halt the run; user must resolve manually.
- **No bidirectional IPC:** Manager ↔ worker is one-shot dispatch. Manager cannot "pivot" a worker mid-task.
- **Provider support:** Autonomous mode requires tool-use capable providers (Anthropic, OpenAI, Gemini, OpenRouter, etc.). Local models without function calling are unsupported.
- **No cost override mid-run:** Cost cap is checked after each turn; run aborts if breached. No "ask user for permission to continue" in v1.

## Acceptance Criteria (M8)

The feature is complete when:

1. **Full end-to-end workflow** on a sample repo:
   - Plan is proposed and user-approved.
   - Workers run in worktrees and complete tasks.
   - Integration branch created with squashed commits.
   - PR opened via `gh` (or push URL printed if `gh` unavailable).

2. **Cross-provider validation:**
   - Acceptance test runs against all 9 providers.
   - No provider-specific tool-call parsing bugs.

3. **CI passes:**
   - GitHub Actions on Ubuntu and Windows.
   - Integration tests with stubbed provider.

4. **Documentation:**
   - README updated with Autonomous Mode section.
   - Roadmap includes M8.

## Open Design Questions

1. **Reviewer placement:** Should the reviewer task gate each task individually, or just the final PR?
2. **Conflict resolution UX:** When a merge conflict occurs, should the run pause and show the conflict to the user, or auto-revert the conflicting task and mark it for retry?
3. **Custom roles:** Should Arc-Code ship with default roles, or encourage users to define their own? (Current plan: ship defaults, make user override easy.)
4. **Manager fallibility:** If the manager agent makes a bad plan, can the user edit it interactively? (Current plan: render + approve/edit/reject; edit opens `$EDITOR` on task JSON.)

## Related Documentation

- See `plan.md` for the full phased implementation roadmap.
- See `ARCHITECTURE.md` for the agent loop and tool dispatch model.
- See `TOOLS.md` for built-in tool reference.
- See `LEARNING-LOOP.md` for how autonomous sessions are embedded and recalled.
