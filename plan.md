# Autonomous Mode — Implementation Plan

A new `arccode autonomous "<goal>"` subcommand that plans a multi-task piece of
work, spawns a manager agent that delegates to specialized worker agents
running in isolated git worktrees, then converges their output into one branch
and opens a PR.

This builds on existing pieces:

- `arccode-core` agent loop, `Provider` trait, streaming events.
- `arccode-tools::spawn_subagent` (will be generalized).
- `arccode worktree create / remove` (worktree management).
- `arccode --print --json` (headless agent loop with NDJSON event stream).
- `arccode review` (uses `gh` for PR diffs — same dependency path).
- `arccode-session` JSONL append-only log format.

---

## Confirmed decisions (from kickoff Q&A)

| Decision           | Choice                                                                 |
| ------------------ | ---------------------------------------------------------------------- |
| Entry point        | New CLI subcommand: `arccode autonomous "<goal>"`                      |
| Approval gates     | Plan approval + PR review only; otherwise hands-off                    |
| Worker execution   | Subprocess per agent (`arccode --print --json` child processes)        |
| Model tiering      | Manager + reviewers on `default_model`; workers on `router.fast_model` |
| Dev branch         | `feature/autonomous-mode` off `main`; per-phase PRs into it; final     |
|                    | PR from `feature/autonomous-mode` into `main`                          |
| Platforms          | Windows **and** Unix in v1 — cross-platform process control from day 1 |
| Providers          | All nine supported providers — Phase 8 smoke-tests each tool-call path |
| Session logs       | Each manager + worker writes its own JSONL under                       |
|                    | `<project>/.arccode/sessions/`; `tasks.jsonl` references by session id |

## Opinionated defaults (flip during review if wrong)

| Area                  | Default                                                                  |
| --------------------- | ------------------------------------------------------------------------ |
| New crate             | `arccode-autonomous` (parallel to `arccode-learn`, `arccode-mcp`)        |
| Run directory         | `<project>/.arccode/autonomous/<run-id>/`                                |
| Task store            | `tasks.jsonl` (append-only) + `state.json` (latest snapshot)             |
| Worker worktrees      | `.arccode/worktrees/auto-<run-id>-<task-slug>/`                          |
| Integration branch    | `arccode/auto/<run-id>` — workers merge here, PR opens from it           |
| Base commit           | `HEAD` at run start; all worktrees branch from this commit               |
| Concurrency cap       | `[autonomous] max_concurrent_agents = 4`                                 |
| Cost cap              | `[autonomous] max_usd = 10.0` — abort run if exceeded                    |
| Per-task timeout      | `[autonomous] task_timeout_secs = 1800`                                  |
| Conflict strategy     | Manager linearizes merges; first conflict → task → `review`, run halts   |
| Failure policy        | One retry with a fresh worker; second failure → `review` + user prompt   |
| Agent roles shipped   | `developer`, `designer`, `tester`, `reviewer` (manager is implicit)      |
| Role definition       | Markdown files at `~/.arccode/agents/<role>.md` (with system prompt)     |
| PR creation           | `gh pr create` — falls back to "push + print URL" if `gh` missing        |

---

## User-facing surface

### CLI

```text
arccode autonomous <GOAL> [OPTIONS]

  <GOAL>                         The high-level objective in natural language.

  --plan-only                    Plan and write tasks.jsonl, do not spawn workers.
  --resume <RUN_ID>              Resume an interrupted run.
  --max-agents <N>               Override [autonomous].max_concurrent_agents.
  --max-usd <FLOAT>              Override [autonomous].max_usd cap.
  --no-pr                        Skip `gh pr create` (just push the branch).
  --yes                          Auto-approve the plan (no interactive gate).
  --base <REV>                   Branch from <REV> instead of HEAD.
```

### Run lifecycle, from the user's perspective

```
$ arccode autonomous "add dark-mode toggle to the TUI"

[autonomous] planning…
[autonomous] proposed 7 tasks (run id: 2026-05-27-1430-a3f).
  1. [developer] Add `theme.mode` field to tui config (deps: —)
  2. [developer] Wire toggle key (`Ctrl+T`) into composer
  3. [designer]  Define dark palette in arccode-tui::theme
  …
  7. [reviewer]  Final review + changelog entry

Approve plan? [y / e (edit) / n] y

[autonomous] spawning manager…
[autonomous] manager → developer #1  worktree=auto-…-task-1
[autonomous] manager → designer  #3  worktree=auto-…-task-3
[autonomous] task 1 done (developer, 2m18s, $0.07)
[autonomous] task 3 done (designer,  3m02s, $0.11)
…
[autonomous] all tasks done. merging worktrees into arccode/auto/<run-id>…
[autonomous] PR opened: https://github.com/vedantnimbarte/Arc-Code/pull/42
```

### TUI dashboard

When the user runs `arccode` (no subcommand) and an autonomous run is active in
the cwd, a new top-bar entry **`Autonomous: <run-id> · 3/7 done`** is shown
and `Ctrl+A` opens a dedicated split-pane view:

```
┌─ Tasks ─────────────────────┬─ Agents ──────────────────────┐
│ #1  developer  done         │ agent-7f3a  developer  task#5 │
│ #2  developer  in-progress  │ agent-9c1b  designer   idle   │
│ #3  designer   done         │ agent-2d44  tester     task#6 │
│ …                           │                               │
├─ Live log ──────────────────┴───────────────────────────────┤
│ 14:32:11  task#5 developer: edit_file crates/…/composer.rs  │
│ 14:32:14  task#6 tester:    run_shell cargo test -p arccode │
│ …                                                           │
└─────────────────────────────────────────────────────────────┘
```

Three new slash commands:

- `/autonomous status` — print the current run summary.
- `/autonomous abort` — terminate manager and all workers, leave worktrees in place.
- `/autonomous resume` — re-attach the dashboard to a running orchestrator.

---

## Data model

### Session logs (per-agent JSONL, reused infra)

Each manager and worker subprocess is run with session logging enabled, so
`<project>/.arccode/sessions/<session-id>.jsonl` is written for each agent
exactly as a normal headless run would. The autonomous layer:

- Assigns each agent a session id at spawn time and passes it to the child
  via env var (`ARCCODE_SESSION_ID`).
- Records `agent.session` events in `tasks.jsonl` that point at the
  session id — so `state.json` always knows where to find the full
  turn-by-turn for any agent.
- This means `arccode session fork <id>` works on an autonomous worker's
  session, and `recall_session` will surface autonomous-mode work in
  future runs through the existing learning loop.

### `tasks.jsonl` (append-only event log)

Each line is one event. State is reconstructed by replaying events on load.

```jsonc
{"t":"2026-05-27T14:30:01Z","ev":"task.create","id":"t1","role":"developer","title":"Add theme.mode field","deps":[],"goal":"…","acceptance":"…"}
{"t":"…","ev":"task.assign","id":"t1","agent":"agent-7f3a","worktree":"auto-…-t1"}
{"t":"…","ev":"task.status","id":"t1","status":"todo"}
{"t":"…","ev":"task.status","id":"t1","status":"in_progress"}
{"t":"…","ev":"task.tool","id":"t1","agent":"agent-7f3a","tool":"edit_file","input_hash":"…","ok":true}
{"t":"…","ev":"task.status","id":"t1","status":"review","outcome":{"summary":"…","commits":["abc123"],"files_changed":4}}
{"t":"…","ev":"task.status","id":"t1","status":"done"}
{"t":"…","ev":"agent.usd","agent":"agent-7f3a","model":"…","input_tokens":1234,"output_tokens":456,"usd":0.07}
{"t":"…","ev":"run.merge.start","branch":"arccode/auto/<run-id>"}
{"t":"…","ev":"run.merge.task","id":"t1","strategy":"squash","commit":"def456"}
{"t":"…","ev":"run.pr","url":"https://github.com/…/pull/42"}
{"t":"…","ev":"run.done"}
```

Statuses: `pending` (created, deps not met) → `todo` (deps met, awaiting
agent) → `in_progress` (agent working) → `review` (agent reported complete,
awaiting integration) → `done` (merged into integration branch) | `failed` |
`blocked`.

### `state.json` (latest snapshot, written atomically after each event)

```jsonc
{
  "run_id": "2026-05-27-1430-a3f",
  "goal": "add dark-mode toggle to the TUI",
  "base_commit": "346077d…",
  "integration_branch": "arccode/auto/2026-05-27-1430-a3f",
  "status": "running",
  "tasks": [
    {"id":"t1","role":"developer","title":"…","status":"done","deps":[],"agent":"agent-7f3a","worktree":"…","usd":0.07,"commits":["abc123"]},
    …
  ],
  "agents": [
    {"id":"agent-7f3a","role":"developer","current_task":"t5","pid":12345,"status":"in_progress"},
    …
  ],
  "totals": {"usd": 0.42, "tokens_in": 12345, "tokens_out": 4567}
}
```

---

## Architecture

```
                       ┌───────────────────────┐
   arccode autonomous  │ arccode-cli           │  parses subcommand,
   "add dark-mode…"    │  ::autonomous_main()  │  loads config, picks run-id
                       └──────────┬────────────┘
                                  │
                                  ▼
                       ┌───────────────────────┐
                       │ arccode-autonomous    │
                       │  ::Orchestrator       │  plan → approve → spawn manager
                       │                       │  → schedule workers → merge → PR
                       └──────────┬────────────┘
                                  │
            ┌────────── spawns ───┴─────────────┐
            ▼                                   ▼
   ┌─────────────────┐                ┌─────────────────────┐
   │ manager agent   │                │ worker agent  ×N    │
   │ (in-process     │                │ (child process:     │
   │  agent loop)    │ ── tool ──►    │  arccode --print    │
   │                 │  assign_task   │  --json --mode      │
   │                 │  finalize_task │  auto-edit          │
   │                 │  add_task      │  --worktree <path>  │
   │                 │  message_agent │  --role <role>      │
   │                 │                │  --task-file <p>)   │
   └─────────────────┘                └─────────────────────┘
            │                                   │
            └──────── both write events ────────┘
                                  │
                                  ▼
                       ┌───────────────────────┐
                       │ tasks.jsonl           │
                       │ state.json            │
                       └───────────────────────┘
```

The orchestrator owns the JSONL/state files — neither the manager nor the
workers write them directly. Instead, every state-mutating tool call (manager
tools, worker `task_complete` tool) is routed through an in-process
`RunStore` actor that serializes writes and broadcasts updates to the TUI.

### New crate: `arccode-autonomous`

```
crates/arccode-autonomous/
├── Cargo.toml
└── src/
    ├── lib.rs              # public Orchestrator API
    ├── orchestrator.rs     # run lifecycle, spawning, merge, PR
    ├── planner.rs          # initial planning call to manager
    ├── manager.rs          # manager agent loop + tool registry
    ├── worker.rs           # subprocess supervisor + event parser
    ├── store.rs            # RunStore: tasks.jsonl + state.json
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

### Files touched in existing crates

| File / area                                       | Change                                                                 |
| ------------------------------------------------- | ---------------------------------------------------------------------- |
| `crates/arccode-cli/src/main.rs`                  | Add `Autonomous { goal, … }` subcommand variant + dispatch.            |
| `crates/arccode-cli/src/args.rs` (or equiv.)      | Argument struct for the subcommand.                                    |
| `crates/arccode-cli/src/print_mode.rs` (or equiv.) | Honor new `--worker-mode` + `--task-file` flags when spawned as a worker. |
| `crates/arccode-config/src/lib.rs`                | Add `[autonomous]` config section + serde struct.                      |
| `crates/arccode-core/src/agent.rs` (or equiv.)    | Plumb a `WorkerHooks` so child processes emit `task.tool` events.      |
| `crates/arccode-tools/src/spawn_subagent.rs`      | Generalize: lift depth-1 cap behind an explicit `allow_nested` flag.   |
| `crates/arccode-tui/src/app.rs`                   | Detect active run; add `Ctrl+A` dashboard, `/autonomous *` commands.   |
| `crates/arccode-tui/src/views/autonomous.rs`      | New file: dashboard split-pane view.                                   |
| `Cargo.toml` (workspace root)                     | Add `arccode-autonomous` to `members`.                                 |
| `README.md`                                       | New section under Highlights + Roadmap entry.                          |

---

## Phased implementation

### Phase 1 — Scaffolding & data model

1. Create `arccode-autonomous` crate, add to workspace.
2. Define `Task`, `Agent`, `Run`, `Status`, `Role`, `Event` types in `model.rs`.
3. Implement `RunStore` with append-only JSONL writer + atomic `state.json`
   snapshotter + replay-on-load. Unit-test event replay correctness.
4. Add `[autonomous]` to `arccode-config` (limits, role overrides, branch
   prefix, gh path).

**Done when:** can construct a `RunStore`, append events, kill the process,
reopen, and observe the same state.

### Phase 2 — CLI surface & planner

1. Wire `arccode autonomous <GOAL>` in `arccode-cli`.
2. Implement `planner.rs`: single call to manager model with a system prompt
   templated from `~/.arccode/agents/manager-planner.md` (default shipped
   with the crate, user-overridable). Output: structured JSON list of tasks
   with `role`, `title`, `goal`, `acceptance`, `deps`.
3. Render the plan in the terminal, prompt `y / e / n`. `e` opens `$EDITOR`
   on the task list; user edits, we re-parse.
4. On approval, persist all tasks as `task.create` events.

**Done when:** `arccode autonomous --plan-only "<goal>"` writes a valid
`tasks.jsonl` and exits.

### Phase 3 — Worker subprocess protocol

1. Add a hidden `--worker-mode` flag to `arccode-cli` that:
   - Loads the role's system prompt from `~/.arccode/agents/<role>.md`.
   - Reads task spec from `--task-file <path>` (JSON).
   - Sets `--mode auto-edit`, cwd = worktree path, model = configured
     worker model.
   - Streams `--json` events to stdout (already supported).
   - On agent completion, emits one final
     `{"event":"task_complete","summary":"…","files_changed":[…]}`.
2. Implement `worker.rs`: spawn the child, parse NDJSON, forward
   `task.tool` events into `RunStore`, enforce `task_timeout_secs`, kill
   on timeout/abort. Process control is cross-platform:
   - Unix: spawn child in its own process group (`setsid`) and kill via
     `kill(-pgid, SIGTERM)` then `SIGKILL` after a grace period.
   - Windows: assign the child to a Job Object with
     `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; closing the job handle (or
     calling `TerminateJobObject`) reaps the whole process tree. Fallback
     `taskkill /T /F /PID <pid>` if Job Objects fail.
   Encapsulate this in a small `child_process::Supervisor` abstraction so
   the rest of the orchestrator stays platform-agnostic.
3. Add a `task_complete` tool to the tools registry, gated to worker mode,
   that flushes the final event and terminates the loop cleanly.

**Done when:** a single hardcoded task can be executed end-to-end in a
worktree, with events streamed into `tasks.jsonl`, and a clean exit.

### Phase 4 — Manager agent + scheduling

1. Implement `manager.rs`: an in-process `arccode-core` agent loop using the
   manager model and a tool registry restricted to:
   `add_task`, `assign_task`, `reassign_task`, `finalize_task` (move
   `review → done` after merge), `message_agent`, `abort_task`, plus
   read-only inspection tools (`list_dir`, `read_file`, `grep_tool`).
2. Manager system prompt is loaded from
   `~/.arccode/agents/manager.md` (default shipped, user-overridable).
3. Manager runs in a loop: scan `state.json`, pick eligible tasks (deps met,
   under concurrency cap), call `assign_task` → orchestrator spawns worker.
4. Orchestrator wakes manager whenever a task moves to `review` or `failed`
   so it can react.

**Done when:** a 3-task plan with one dependency edge runs to completion
with the manager correctly waiting on the dep.

### Phase 5 — Worktree integration & merge

1. `worktree.rs`: for each worker, create
   `.arccode/worktrees/auto-<run-id>-<task-slug>/` from `base_commit` on a
   branch named `arccode/auto/<run-id>/<task-slug>`.
2. After each worker exits cleanly, run `git -C <wt> add -A && git commit`
   if there are unstaged changes (worker is also expected to commit, but
   belt-and-braces).
3. When all tasks are `review`, orchestrator:
   - Creates integration branch `arccode/auto/<run-id>` from `base_commit`.
   - Linearizes tasks by dep order, then by id.
   - For each task: `git merge --squash <task-branch>` + commit with
     message `<task.title>\n\n<task.outcome.summary>`.
   - On conflict: mark task `blocked`, write a `run.conflict` event with
     conflict file list, halt the run, surface to user.
4. On success: cleanup worktrees, keep the integration branch.

**Done when:** a clean 3-task run produces three squashed commits on the
integration branch and removes all worker worktrees.

### Phase 6 — PR creation

1. `pr.rs`: detect `gh` on `PATH`; if present, run
   `gh pr create --base <main> --head <integration-branch>` with a body
   templated from the plan + per-task outcomes.
2. If `gh` missing or unauthenticated: `git push -u origin
   <integration-branch>` and print the GitHub compare URL.
3. Write `run.pr` event, then `run.done`.

**Done when:** end-to-end run on a sample repo opens a PR (or prints the
push URL) and the run terminates cleanly.

### Phase 7 — TUI dashboard

1. New view `crates/arccode-tui/src/views/autonomous.rs` with three panes
   (Tasks, Agents, Live log).
2. App boot: scan `.arccode/autonomous/*/state.json` for runs in non-terminal
   states; if any, surface the top-bar indicator.
3. `Ctrl+A` toggles the dashboard. `/autonomous {status,abort,resume}`.
4. Dashboard subscribes to `RunStore` broadcast channel — every appended
   event triggers a redraw.

**Done when:** running the TUI while a background `arccode autonomous` is
active shows live progress without polling.

### Phase 8 — Cross-provider validation, failure handling, polish

1. Per-task timeout (kill + retry once with a fresh worker).
2. Cost cap (`max_usd`) checked after every `agent.usd` event; on breach,
   abort all workers and mark run `failed`.
3. `--resume <RUN_ID>`: replay state, restart missing workers for
   `in_progress` tasks (those whose pid is gone or unresponsive).
4. **Provider validation matrix.** Run the acceptance test (a tiny canned
   plan) against each of the nine providers and confirm the worker
   tool-call shape is parsed correctly end-to-end. Concretely:
   - Anthropic — native tool use (reference).
   - OpenAI — `tool_calls` / `function_call` shape.
   - ChatGPT (OAuth) — same shape as OpenAI, plus token refresh path.
   - Gemini — `functionCall` shape.
   - OpenRouter, LiteLLM, LM Studio, vLLM, Ollama — OpenAI-compat shape;
     test with one model per backend that supports tool use.

   Any provider that can't reliably emit tool calls (some local models)
   is marked **unsupported for autonomous mode** in README and the
   subcommand errors out early with a helpful message if selected.
5. README updates: new "Autonomous mode" section, Roadmap M7 entry, and
   a provider-support table for autonomous mode specifically.
6. End-to-end integration test using a tiny scratch repo and a stubbed
   provider that returns canned tool calls.
7. Cross-platform CI: GitHub Actions matrix runs the integration test on
   `ubuntu-latest` and `windows-latest`.

---

## Enhancements — reduce developer interaction & raise throughput

The phases above ship the minimum viable autonomous loop. The enhancements
below are layered on top to cut the two remaining interaction points (plan
approval, PR review) toward zero and to make the loop self-healing.

### E1. Trust-tiered auto-approval (kills the plan-approval gate)

Replace the unconditional `y / e / n` prompt with a risk classifier on the
proposed plan. Config:

```toml
[autonomous.approval]
auto_approve_usd        = 1.00         # est. cost ceiling for auto
auto_approve_max_tasks  = 5
auto_approve_globs      = ["crates/**/*.rs", "docs/**", "README.md"]
dangerous_paths         = ["**/migrations/**", ".github/**", "**/auth/**",
                           "**/secrets*", "Cargo.lock"]
notify_only_window_secs = 60           # "veto in 60s" for medium-risk
notify_channel          = "desktop"    # desktop | slack:<webhook> | none
```

Tiers:

- **auto** — plan ≤ `auto_approve_max_tasks`, all writes match
  `auto_approve_globs`, est. cost < `auto_approve_usd`, no `dangerous_paths`
  hit. Proceeds silently.
- **notify-only** — fires a notification with the plan summary; proceeds
  unless vetoed within `notify_only_window_secs`.
- **hard gate** — falls back to the existing `y / e / n` prompt.

`--yes` forces auto; `--review` forces hard gate.

### E2. Two-pass, repo-aware planner

1. **Grounding pass** (cheap, fast model): `recall_session` + targeted
   `grep`/`list_dir` over the goal's keywords. Produces a "facts" block:
   real file paths, existing symbols, prior related work.
2. **Draft pass**: planner emits a plan conditioned on the facts block.
3. **Critique pass**: same model re-reads its own plan against a checklist:
   - Every referenced path exists.
   - Every `acceptance` is an executable command.
   - Dep graph is acyclic and connected.
   - No two tasks have overlapping `writes` (see E3).
4. **Rewrite pass**: planner rewrites once based on the critique.

Net effect: dramatically fewer hallucinated modules and untestable tasks.
Adds ~2–3× planner tokens but the planner is a tiny fraction of total cost.

### E3. Executable acceptance criteria + self-verification

Schema change for tasks:

```jsonc
{
  "ev": "task.create", "id": "t1", "role": "developer",
  "title": "Add --version-only flag",
  "goal": "…",
  "writes": ["crates/arccode-cli/src/main.rs",
             "crates/arccode-cli/src/args.rs"],
  "acceptance": [
    {"kind": "shell", "cmd": "cargo check -p arccode-cli"},
    {"kind": "shell", "cmd": "cargo test -p arccode-cli version_only"},
    {"kind": "grep",  "pattern": "version-only", "path": "crates/arccode-cli/src/args.rs"}
  ]
}
```

Workers must run every acceptance check and attach results to
`task_complete` before transitioning to `review`. Failed acceptance → task
auto-loops back into the retry ladder (E5). Green acceptance lets the
reviewer skip re-verifying mechanical checks.

### E4. Conflict avoidance via write-set scheduling + rebase-as-you-go

Replace the "linearize merges at the end, halt on first conflict" strategy:

1. **Write-set constraint in the scheduler**: never run two tasks whose
   `writes` globs overlap concurrently. Planner is required to declare
   them (E3); critique pass enforces non-overlap inside a concurrency
   wave.
2. **Continuous integration branch**: orchestrator merges each task into
   `arccode/auto/<run-id>` the moment the task hits `review` + passes
   acceptance. Later workers branch from / rebase onto the latest
   integration tip instead of the original base commit.
3. **Auto-merge-fixer subagent**: on conflict, spawn a dedicated worker
   with role `merge-fixer` whose only job is to resolve the conflict and
   re-run acceptance. Only escalate to the user if the fixer fails.

This converts most "halt the run" events into transparent recoveries.

### E5. Structured failure retry ladder (self-healing)

Replace the flat "1 retry → user prompt" policy with:

| Rung | Action                                                            |
| ---- | ----------------------------------------------------------------- |
| 1    | Same worker, same model, failure diff + acceptance output appended to context |
| 2    | Fresh worker, escalate model (`router.fast_model` → `default_model`), full task history attached |
| 3    | **Splitter call**: planner-style call that decomposes the failing task into 2–3 smaller tasks; re-enqueue |
| 4    | Mark `blocked`, surface to user with full context                 |

Between every worker turn (not just at task end), the orchestrator runs
`cargo check` (or project-configured `[autonomous].turn_gate_cmd`) inside
the worktree. Red turns are rolled back via the checkpoint (E11) and the
worker is re-prompted with the failure — keeps bad turns from compounding.

### E6. Cross-run learning loop

Leverage existing `recall_session` / session-log infrastructure:

- **Planner priming**: before E2's draft pass, fetch top-K similar past
  runs by goal-embedding similarity; inject their plans + final outcomes
  (merged / reverted / abandoned) as in-context examples.
- **Per-role lessons file**: `~/.arccode/agents/<role>.lessons.md` —
  appended to whenever a task by that role is reverted in PR review or
  rewritten heavily by a later commit. Loaded into the role's system
  prompt on subsequent runs.
- **Adaptive model routing**: track first-try success rate per
  `(role, task_kind, model)` tuple in `~/.arccode/stats.jsonl`; the
  scheduler picks the cheapest model whose historical success rate
  exceeds a threshold, instead of statically using `router.fast_model`
  for all workers.

### E7. Reviewer-per-task (replaces end-of-run reviewer)

(Promotes Open Question #4 to a decision.)

Add a status: `in_progress → review → reviewing → done | rework`.

- When a worker reports `review` + green acceptance, orchestrator
  immediately spawns a reviewer agent in parallel with the next eligible
  worker. Reviewer has read-only tools + the diff for that one task.
- Reviewer outcomes: `approve` → `done` + merge; `rework` → task returns
  to `todo` with reviewer notes appended.
- A single final reviewer still runs on the integration branch for
  cross-cutting concerns (changelog, release notes), but per-task
  reviewers catch issues at the cheapest possible point.

This is the change that lets the human PR review become a rubber stamp.

### E8. PR-side automation (so human review is a rubber stamp)

Before notifying the user that the PR is ready:

1. Run `arccode review` on the integration branch; post findings as
   inline PR comments via `gh pr review --comment`.
2. Auto-generate the PR body sections:
   - **Summary** — from the goal + per-task outcome summaries.
   - **Test plan** — concatenation of every task's `acceptance` commands,
     pre-checked.
   - **Changelog entry** — derived from squash commit messages.
   - **Visual evidence** for TUI changes: render the affected views to
     SVG via ratatui's test backend, attach as PR images.
   - **What to scrutinize** — auto-flagged list of files matching
     `dangerous_paths`, plus any task that took >1 retry rung.
3. **Auto-merge** when: tier was `auto` (E1), CI is green, no
   `dangerous_paths` touched, and `arccode review` finds nothing
   severity ≥ `medium`. User is notified post-merge with a link.

Config:

```toml
[autonomous.pr]
auto_merge          = true
auto_merge_max_severity = "low"
require_ci_green    = true
```

### E9. Throughput: speculative execution + adaptive concurrency

- **Speculative dispatch**: when a worker is mid-flight on task `t_n`,
  pre-spawn a fast-model worker on the most-likely-next task `t_{n+1}`
  using current state. If the manager confirms the assignment, promote;
  otherwise discard. Hides spawn + planning latency.
- **Idle-reviewer fan-out**: each `review` transition spawns its reviewer
  immediately (E7), in parallel with continued worker execution.
- **Adaptive concurrency cap**: replace static `max_concurrent_agents = 4`
  with a controller that scales between `[min, max]` based on:
  - per-provider rate-limit headroom (parse 429s and `Retry-After`),
  - host CPU load,
  - current `usd_spent / max_usd` burn rate.

### E10. Manager↔worker bidirectional comms (promotes Open Q #2)

Implement `message_agent` properly in Phase 4 — not Phase 4-stub. Workers
expose a stdin command channel; manager can send:

- `pivot` — append new context + revised goal mid-task.
- `cancel` — abort cleanly, commit partial work to a side branch.
- `clarify` — inject answer to a question the worker raised.

Workers can also push `question` events the manager can answer without
killing the task. Eliminates most "restart from scratch on drift" cases.

### E11. Mandatory checkpoint hygiene (promotes Open Q #5)

Worker system prompt mandates `arccode checkpoint` before any
multi-file edit and after each acceptance-green milestone. Orchestrator
verifies via the session log that at least one checkpoint exists before
allowing a task to enter `review`. Rollback (E5 turn-gate) uses the
nearest prior checkpoint.

### E12. `--watch` mode (low-cost UX win)

`arccode autonomous --watch "<goal>"` runs the orchestrator and tails
the run with a minimal terminal progress UI (reuse the event stream from
the TUI dashboard but render flat). For users who want to observe a run
without opening the full TUI. Default behavior remains background-style
streaming as in the current plan.

### E13. Drop `designer` from v1; add `refactorer` and `merge-fixer`

(Promotes Open Question #1 to a decision.)

Shipped roles: `developer`, `tester`, `reviewer`, `refactorer`,
`merge-fixer`. `designer` deferred until there's a concrete artifact
it produces on a TUI codebase. `refactorer` exists because the splitter
ladder (E5 rung 3) often produces "extract helper" tasks that are
better routed to a refactor-specialized prompt than to `developer`.

---

## Revised defaults table

These overrides replace the corresponding rows in "Opinionated defaults":

| Area                  | Revised default                                                          |
| --------------------- | ------------------------------------------------------------------------ |
| Approval flow         | Trust-tiered (E1); hard gate only for risky plans                        |
| Conflict strategy     | Write-set scheduling + rebase-as-you-go + auto merge-fixer (E4)          |
| Failure policy        | 4-rung retry ladder with auto-splitting (E5); per-turn check-gate        |
| Agent roles shipped   | `developer`, `tester`, `reviewer`, `refactorer`, `merge-fixer` (E13)     |
| Reviewer placement    | Per-task reviewer (E7); final reviewer only for cross-cutting concerns   |
| PR finalization       | Auto-`arccode review` + auto-generated body + conditional auto-merge (E8) |
| Manager↔worker IPC    | Bidirectional via stdin command channel (E10)                            |
| Checkpoint policy     | Mandatory before multi-file edits; enforced by orchestrator (E11)        |

---

## Revised phasing (enhancements folded in)

Phases 1–7 ship as written. Insert the following before Phase 8:

### Phase 7.5 — Self-healing & low-interaction core

1. **E3** — `writes` + executable `acceptance` schema; worker
   self-verification; orchestrator enforcement.
2. **E5** — 4-rung retry ladder + per-turn check-gate + rollback to
   nearest checkpoint.
3. **E11** — checkpoint enforcement.
4. **E10** — bidirectional manager↔worker IPC.

**Done when:** the acceptance test (canned `--version-only` plan)
survives one injected failure per rung without user intervention.

### Phase 7.6 — Planner quality

1. **E2** — two-pass, repo-aware planner.
2. **E13** — role lineup updated.
3. Planner emits `writes` + `acceptance` arrays (depends on E3).

**Done when:** planner-emitted file paths exist in the repo 100% of the
time across a 20-goal benchmark.

### Phase 7.7 — Conflict avoidance & throughput

1. **E4** — write-set scheduling + rebase-as-you-go + merge-fixer role.
2. **E9** — speculative dispatch + adaptive concurrency.
3. **E7** — reviewer-per-task.

**Done when:** a 7-task plan with two overlapping-write tasks completes
without halting and without manual merge intervention.

### Phase 7.8 — Trust tier, PR automation, UX

1. **E1** — trust-tiered approval + config.
2. **E8** — `arccode review` on integration + auto-PR-body +
   conditional auto-merge.
3. **E12** — `--watch` mode.

**Done when:** acceptance test runs with no user input from invocation
through merged PR.

### Phase 7.9 — Cross-run learning

1. **E6** — planner priming from past runs, per-role lessons files,
   adaptive model routing.

**Done when:** a goal re-run after a revert demonstrably avoids the
reverted approach (verify against a seeded "trap" test case).

Phase 8 (cross-provider validation + CI matrix) runs last, unchanged.

---

## Toward fully autonomous operation ("Jarvis mode")

Everything above gets us to *"give the agent a goal, walk away, come back to a
merged PR."* Jarvis-mode goes further: the agent **finds work on its own,
challenges goals it thinks are wrong, proposes better approaches, runs
continuously, talks back through whatever channel you use, and grows new
capabilities as it needs them.**

Reality check before piling on: an LLM agent will never be Tony Stark's
Jarvis — it has no real-time world model and will confidently hallucinate
under pressure. The enhancements below are the achievable approximation:
high agency *within* a well-instrumented sandbox, with cheap verification
loops and humans in the loop only at decision boundaries that actually
matter.

### J1. Goal refinement & negotiation loop (before planning)

Today the planner accepts the goal as-is. Add a refinement stage that runs
*before* E2's planner:

1. **Clarify pass** — agent reads the goal, scans the repo, and either:
   - emits `clarifying_questions` (max 3, only if the answer materially
     changes the plan), or
   - emits a `goal_restatement` ("I think you mean X — confirm or correct")
     when the goal is ambiguous but inferable.
2. **Challenge pass** — agent evaluates the goal against the codebase and
   may push back:
   - "This conflicts with the in-progress refactor on `feature/auth-v2`."
   - "There's already a `--quiet` flag — `--version-only` would duplicate
     behavior. Extend that flag instead?"
   - "The simpler path is X; the goal as stated would take ~3× longer."
3. **Better-approach suggestions** — up to 2 alternatives ranked by
   estimated cost/time/risk, with one-line tradeoffs. User picks or sticks
   with the original.

Auto-tier (E1) allows the agent to silently *accept* its own restatement
when confidence is high; medium confidence triggers a notify-only window;
low confidence escalates.

Config:

```toml
[autonomous.refine]
max_clarifying_questions = 3
challenge_threshold      = "medium"   # off | low | medium | high
suggest_alternatives     = true
```

### J2. Autonomous goal discovery (daemon mode)

A new long-running mode that finds work without being asked:

```bash
arccode daemon                      # runs in background, watches repo + signals
arccode daemon status
arccode daemon stop
```

The daemon polls/subscribes to:

- **GitHub issues** labeled `arccode:auto` (or configured label).
- **GitHub PRs** failing CI, with dependabot PRs as a special case.
- **Failing scheduled jobs** (read recent CI runs via `gh run list`).
- **TODO / FIXME / XXX** comments added in recent commits, scored by age
  and proximity to changed code.
- **Test coverage gaps** for files modified in the last N days.
- **Stale dependencies** flagged by `cargo outdated` / `npm audit`.

For each candidate, the daemon scores it on (value × confidence ÷ risk)
and either:

- **auto-runs** if the score clears `[autonomous.daemon].auto_threshold`
  *and* the source channel is trusted (issue from an allow-listed author,
  or generated by the daemon itself),
- **proposes** in the configured notify channel ("I'd like to fix #142
  — estimated $0.30, 4m, low risk — reply 👍 to start"),
- **logs and ignores** otherwise.

Config:

```toml
[autonomous.daemon]
enabled              = false
poll_interval_secs   = 300
auto_threshold       = 0.75
max_concurrent_runs  = 2
trusted_authors      = ["vedantnimbarte"]
trusted_labels       = ["arccode:auto"]
sources              = ["github_issues", "ci_failures", "dependabot",
                        "todos", "coverage_gaps"]
```

### J3. Multi-channel intake (talk to it from anywhere)

Goals shouldn't only arrive via CLI. Pluggable intake adapters:

| Channel        | Trigger                                                |
| -------------- | ------------------------------------------------------ |
| CLI            | `arccode autonomous "<goal>"` (already in plan)        |
| GitHub issue   | New issue with `arccode:auto` label                    |
| GitHub comment | `/arccode <goal>` comment on issue or PR by trusted user |
| Slack          | `@arccode <goal>` mention, or DM                       |
| Email          | Mail to `arccode+<repo>@<your-domain>` with goal in body |
| Webhook        | `POST /goals` to daemon's local HTTP endpoint          |
| File drop      | Write `goal.md` into `.arccode/inbox/`; daemon picks it up |

Each adapter normalizes to a `Goal { text, source, author, trust_level }`
struct and feeds the daemon's queue. Same auto/notify/gate tiers apply.

### J4. Conversational mid-run interjection

Once a run is in flight, the user should be able to redirect without
killing it. Reuse E10's manager↔worker IPC and extend up to the user:

```bash
arccode autonomous tell <run-id> "skip the changelog task, we'll do that manually"
arccode autonomous ask  <run-id> "what files have you touched so far?"
```

Or in any intake channel: a reply in the same Slack thread / GitHub
comment thread routes to the active run. Manager handles incoming
messages between tool calls; can re-plan, abort tasks, or answer
questions without restarting.

### J5. Proactive status reporting (push, don't poll)

Daemon emits proactive updates instead of waiting to be asked:

- **Per-run**: start, mid-run if exceeding 50% of estimated cost, on
  completion, on failure.
- **Daily standup** (configurable cron): "Yesterday: 3 PRs merged, 1
  blocked on review, $1.42 spent. Today's queue: 2 issues triaged."
- **Weekly summary**: trends in cost, success rate, top blockers, suggested
  config tweaks ("you've vetoed 4/5 medium-risk auto plans — consider
  raising `challenge_threshold`").

All reports go to the same notify channel(s) configured for intake.

### J6. Real verification — run the app, don't just test it

Per-task acceptance (E3) is mostly shell commands. For UI/feature work,
add verification kinds that actually exercise the change:

```jsonc
"acceptance": [
  {"kind": "shell",  "cmd": "cargo test -p arccode-tui"},
  {"kind": "run",    "target": "tui", "script": "screenshots/dark-mode.script"},
  {"kind": "assert", "screenshot": "screenshots/dark-mode.png",
                     "must_contain_text": ["Dark mode on"]},
  {"kind": "http",   "url": "http://localhost:3000/api/version",
                     "must_match": {"version": "*"}}
]
```

Reuses the existing `run` and `verify` skills' patterns. For TUI:
ratatui test backend rendered to SVG, diffed against a baseline (or
LLM-judged for "is this dark mode?"). For web: headless browser via the
existing browser tooling. **Workers must run these before `task_complete`.**
This catches the "tests pass but the feature is broken" failure mode that
makes pure-test verification untrustworthy.

### J7. Tool synthesis (agent grows its own capabilities)

When a worker repeatedly hits the same gap ("I keep needing to query
the SQLite DB but there's no tool"), it can propose a new tool:

1. Worker emits `propose_tool { name, description, schema, impl_sketch }`.
2. Orchestrator queues this as a `meta` task in the next run (or
   immediately, in daemon mode).
3. A `tool-smith` role generates the tool implementation in
   `~/.arccode/tools/<name>.{ts,py,rs}`, with a test, and registers it.
4. Next run, the tool is available to all workers.

Gated behind `[autonomous.tools].allow_synthesis = true`. New tools are
sandboxed (E10's IPC + J11's sandbox tier) until reviewed.

This is what makes the agent feel like it's *learning the project* across
runs rather than starting from scratch each time.

### J8. Project knowledge graph (durable memory, beyond session logs)

Session logs (Phase 0 infra) are turn-by-turn and per-run. Add a
**project-scoped knowledge layer** at `.arccode/knowledge/`:

- `architecture.md` — auto-maintained module map, regenerated when
  `crates/*/src/lib.rs` changes.
- `conventions.md` — extracted patterns ("error handling uses
  `anyhow::Result`", "tests live in `#[cfg(test)] mod tests`").
- `decisions.jsonl` — append-only log of architectural decisions taken
  by autonomous runs, with rationale ("chose squash-merge per task
  because rebase-as-you-go caused 3 conflicts in run X").
- `glossary.md` — domain terms with definitions, extracted from code +
  PR descriptions.
- `hotspots.json` — files most-edited / most-conflicted, used by the
  scheduler to bias write-set conflict avoidance.

Maintained by a low-priority `knowledge-keeper` agent that runs after
every merged autonomous PR. The planner (E2) and clarify pass (J1) read
from this layer before generating anything.

### J9. Upfront cost / time / risk estimation with confidence

Before any plan is approved (E1) or auto-run (J2), produce:

```
Estimated: 4–7 tasks · 8–15 min wall · $0.30–$0.80 · risk: low
Confidence: medium (similar past runs: 12 hits, 8 successful first-try)
```

Sources: J8's `decisions.jsonl` + E6's per-role stats + planner's
self-reported uncertainty. Confidence bands matter more than point
estimates — auto-approve only fires when the upper bound of the cost
range is under the cap.

### J10. Critic agent (always-on red team)

A second model runs in parallel to the planner and reviewer, with a
single job: **disagree productively.** Specifically:

- After planning: "what would break this plan?" emits a list of risks;
  any risk above threshold is appended to the plan as a guardrail task.
- After each task review: independent re-review, focused on what the
  primary reviewer is most likely to miss (security, perf, data loss).
- Before auto-merge: final critic pass; any "high severity" finding
  vetoes auto-merge regardless of E8's severity-based gating.

Use a *different model family* than the primary (e.g., if primary is
Claude, critic is GPT or Gemini) — uncorrelated errors catch more.

### J11. Sandboxed execution tiers (risky operations don't touch your machine)

Trust tier (E1) is about *whether* to approve; sandbox tier is about
*where to execute*:

| Tier        | Where                                     | When                                     |
| ----------- | ----------------------------------------- | ---------------------------------------- |
| `host`      | Current machine, current worktree (today) | Default for low-risk runs                |
| `container` | Docker container with repo mounted RO     | Runs touching deps, build scripts, CI    |
| `vm`        | Ephemeral microVM / cloud sandbox         | Migrations, infra changes, untrusted goals |
| `replay`    | Same-container *re-run* of a prior plan   | Verifying determinism / catching flakes  |

Workers in `container`/`vm` tiers stream changes back as patches, applied
to the host worktree only after acceptance + review. Removes the entire
class of "agent ran `rm -rf` in the wrong directory" failures.

Picked per-task by the planner based on `writes` + acceptance commands;
overridable via `[autonomous.sandbox]` defaults.

### J12. Skill packs (shareable, versioned agent definitions)

Today: role definitions in `~/.arccode/agents/<role>.md`. Make them
shareable:

```toml
[autonomous.skills]
packs = [
  "arccode-official/rust-developer@1.4",
  "arccode-official/security-reviewer@2.0",
  "vedantnimbarte/arccode-tui-designer@0.3",
]
```

A pack is a directory: role markdown + lessons file + tool registrations
+ acceptance templates. Installable from a git repo or local path.
Pinned by semver. Lets the community share well-tuned agent definitions
the way Claude Code already does with skills.

### J13. Always-on watcher mode (reacts, doesn't poll)

Subset of J2 specialized for *reactive* work — sits in the repo via
filesystem watcher + git hooks + webhook listener and reacts to events
in real time:

- New failing test on `main` → spin up a fixer run.
- Dependabot PR with green CI → auto-review + auto-merge if within
  allowlisted paths.
- New issue with `arccode:auto` → triage immediately, not on next poll.
- Local file save with `// ASK: <question>` comment → spawn a quick
  research worker; reply inline as comment.

This is the closest thing to "Jarvis is just *there*, listening." It's
J2 with sub-second latency for specific high-value triggers.

### J14. Voice intake (optional, opt-in)

For the "talk to it" feel: a tiny local STT shim (whisper.cpp or
platform-native) bound to a hotkey that captures speech, transcribes,
and dispatches to the daemon's intake queue. Output read back via TTS
or just routed to the notify channel.

This is mostly UX gloss — useful for kicking off goals while
context-switching, not for actual control. Behind
`[autonomous.intake.voice].enabled = false` by default.

### J15. Honest limits & escalation triggers

To stay trustworthy, the daemon must know when to *stop* and ask:

Hard escalation triggers (always interrupt the user, regardless of tier):

- Net negative test count after a task.
- Any change to `dangerous_paths` (E1) without explicit goal mentioning it.
- Detected secrets in a diff (regex + entropy check).
- Cumulative spend exceeds `max_usd` × 0.8 (warn) or × 1.0 (halt).
- 3 consecutive failed runs on related goals — likely the agent is stuck
  in a wrong mental model and needs human reset.
- License / copyright headers being modified.
- Force-push to any non-`arccode/auto/*` branch.

These are non-negotiable — no config flag disables them. Trust is built by
the daemon visibly stopping at these lines, not by maximizing autonomy.

---

## Phasing for Jarvis-mode enhancements

Layered on top of Phases 1–7.9 + 8. None of these block the v1 ship; they
form the v2/v3 roadmap.

### Phase 9 — Negotiation & verification (J1, J6, J9, J15)

Goal: the agent challenges bad goals, runs the app to verify, estimates
honestly, and knows when to stop.

**Done when:** running with a deliberately-wrong goal triggers a challenge;
running a UI change verifies via screenshot diff before opening PR; cost
estimates land within ±30% of actuals on a 20-run benchmark.

### Phase 10 — Always-on daemon (J2, J3, J5, J13)

Goal: `arccode daemon` discovers, proposes, executes, and reports without
being invoked per-run.

**Done when:** daemon runs for a week on a real repo, opens ≥5 PRs from
issues without per-PR human intervention, and the false-positive rate
on proposed work is < 20%.

### Phase 11 — Memory & critic (J8, J10)

Goal: project knowledge accumulates; a red-team critic catches what the
primary reviewer misses.

**Done when:** critic vetoes catch a deliberately-planted regression in
an acceptance test; planner cites at least one `decisions.jsonl` entry
in a new plan.

### Phase 12 — Sandboxing & tool growth (J7, J11, J12)

Goal: risky work runs in containers/VMs; the agent proposes and earns
new tools; skill packs are installable.

**Done when:** a migration task runs end-to-end in a container, applies
to host only after approval; a synthesized tool from one run is used
successfully in the next; an external skill pack installs and runs.

### Phase 13 — Conversational + voice (J4, J14)

Goal: mid-run redirection works from any channel; optional voice intake.

**Done when:** a Slack reply mid-run changes the plan without killing
it; voice intake (opt-in) successfully dispatches a goal end-to-end.

---

## What this stack is, and isn't

It **is**: an always-on, multi-channel agent that finds work, challenges
goals, runs in sandboxed isolation, verifies with real execution, learns
across runs, grows new tools, and reports proactively — with a critic
checking its work and hard limits it can't override.

It **isn't**: omniscient, deterministic, or safe to point at production
without the sandboxing tiers (J11) and escalation triggers (J15). The
value comes from the system *visibly* respecting those limits, not from
removing them.

---

## Open questions to revisit during build

These don't block writing code today, but flag them before merging:

1. **Designer agent on a Rust TUI codebase**: what does "designer" actually
   produce? Mockups in markdown? Theme palette TOML? Worth narrowing the
   role definition in `~/.arccode/agents/designer.md` to "ratatui visual
   style + UX flow" rather than "graphic design".
2. **Manager → worker IPC after spawn**: do we need bidirectional comms
   (manager sends "pivot, the schema changed"), or is one-shot dispatch
   enough? Plan currently assumes one-shot; `message_agent` exists as a
   stub but isn't used in Phase 4.
3. **What happens if `gh` opens an interactive auth flow** mid-run? Plan
   currently treats `gh` errors as "fall back to push + print URL". May
   need to pre-flight check `gh auth status` before kicking off so the run
   doesn't get to the end and stall.
4. **Reviewer's place in the DAG**: currently planner inserts a reviewer
   task at the end as a dep on every other task. Should reviewer instead
   review each task before it moves to `done`, gating merges? That's a
   bigger UX change — defer to a v2 once basics work.
5. **Worktree commit hygiene**: each worker commits on its branch.
   Should we require workers to use `arccode checkpoint` before edits so a
   bad agent run is recoverable? Probably yes — add to the worker system
   prompt.

---

## Acceptance test for the whole feature

On a fresh checkout of this repo, run:

```bash
arccode autonomous "add a --version-only flag to arccode-cli that prints the version and exits without loading config"
```

Expected: planner proposes 2–3 tasks (developer for the flag, tester for a
smoke test, reviewer for changelog), user approves, workers run in worktrees,
integration branch `arccode/auto/<run-id>` ends up with 2–3 squashed commits,
and a PR is opened against `main`. Total wall time on a Sonnet/Haiku tier
should be under 5 minutes and under $0.50.
