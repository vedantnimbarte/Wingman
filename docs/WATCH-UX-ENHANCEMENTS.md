# Pilot Watch UI — Enhancement Plan

A backlog of user-friendliness enhancements for `wingman pilot watch` and the
run-control channel, written so a fresh session (with no prior context) can
pick up any item and implement it. Each feature lists the files to touch, the
existing structs/functions to build on, concrete implementation notes, tests,
and an effort estimate.

> Status: the watch TUI, run control (abort/retry/approve/veto), the ASCII
> fallback, log filtering, detail overlay, header meters, mouse support, help
> overlay, and the finish/failure bell are **already shipped** (PRs #10–#14).
> This document is the *next* layer of polish. (The repo-root `plan.md` is the
> separate master "Autonomous Mode" implementation plan — don't confuse them.)

---

## 1. Orientation — where everything lives

| Area | File | Key items |
| --- | --- | --- |
| Interactive TUI | `crates/wingman-cli/src/commands/pilot_watch_tui.rs` | `WatchUi`, `draw`, `run_loop`, all `render_*`, key handling |
| Pilot CLI commands | `crates/wingman-cli/src/commands/pilot.rs` | `watch`, `status`, `run`, `resume`, `control_*`, `pick_run`, `resolve_ascii`, `wait_for_approval`, `run_notify_window` |
| CLI arg definitions | `crates/wingman-cli/src/cli.rs` | `PilotAction` enum + dispatch |
| Dashboard model (shared) | `crates/wingman-autonomous/src/dashboard.rs` | `DashboardModel`, `TaskRow`, `AgentRow`, `HeaderInfo`, `LogRow`, `RunSummary`, `build_model`, `list_runs`, `load_state`, `tail_events`, `state_mtime` |
| Control channel | `crates/wingman-autonomous/src/control.rs` | `ControlCommand`, `ControlReader`, `append`, `control_path` |
| Orchestrator | `crates/wingman-autonomous/src/orchestrator.rs` | `control_watchdog`, `OrchestratorCommand::AbortRun`, `handle_abort_run` |
| Docs | `docs/AUTONOMOUS-MODE.md` | "Dashboard Layout", "Controlling a live run" |

### `WatchUi` (the TUI state object)

Fields (in `pilot_watch_tui.rs`):

- `runs: Vec<RunSummary>` — active runs (+ the watched one) for the sidebar
- `current: usize` — index into `runs` being watched
- `focus: Focus` — enum `{ Runs, Tasks, Log }`; drives arrow keys
- `tasks_sel: usize`, `tasks_len: usize` — Tasks pane selection + count
- `log: LogView` — `{ scroll, follow, severity: SevFilter, query, editing }`
- `detail: Option<String>` — task id whose detail overlay is open
- `help: bool` — `?` overlay open
- `confirm: Option<Confirm>` — `{ prompt: String, cmd: ControlCommand }` modal
- `toast: Option<String>` — transient status line (see A1 — currently never
  auto-dismisses)
- `model: Option<DashboardModel>`, `finished: bool`, `last_mtime`
- `seen_failed: usize`, `bell_finish_sent: bool`, `ring_bell: bool` — bell state
- `frame: u64` — wall-clock animation tick (`started.elapsed()/120`)
- `spend_samples: Vec<u64>` — cents, for the header sparkline
- `hit: HitAreas` — per-frame pane rects for mouse hit-testing
- `glyphs: Glyphs` — `{ ascii: bool }`; call `.pick(uni, ascii)` / `.spinner(frame)`

### Rendering pipeline

`draw(f, ui)` lays out rows `[header(4), meters(3), grid(min), footer(1)]`, then
the grid `[top(62%), log(38%)]`, then top `[Runs sidebar? | Tasks | Agents]`.
Each `render_*` takes plain data + `Glyphs`. Overlays (`render_confirm`,
`render_help`, `render_detail`) draw last via `centered_rect` + `Clear`.

### Keys already bound (in `run_loop`)

`q`/`Esc`/`Ctrl-C` quit · `Tab` cycle focus · `↑/↓`/`k`/`j` nav · `PgUp/PgDn` ·
`Home`/`g` top · `End`/`G` bottom · `Enter` task detail · `/` log search ·
`f` log severity filter · `?` help · `x` abort run (confirm) · `r` retry task
(confirm) · `a`/`v` approve/veto (while awaiting) · `1`–`9` jump to run ·
`y`/`Enter` confirm, any other cancels · mouse wheel/click.

**Free keys** for new features: `t`, `n`, `N`, `c` (non-Ctrl), `b`, `e`, `l`,
`m`, `o`, `p`, `s`, `u`, `w`, `z`, `y` (yank), and any Shift/`Ctrl` combos.

### Dashboard model shapes (what the TUI renders)

- `TaskRow { id, role, title, status: TaskStatus, deps: Vec<String>, writes: usize, usd: f64, elapsed_secs: Option<i64>, attempts: u32, agent: Option<String> }`
- `AgentRow { id, name, role, status: AgentStatus, task: Option<String>, pid: Option<u32>, tool: Option<String>, uptime_secs: Option<i64>, usd: f64 }`
- `HeaderInfo { run_id, status: RunStatus, done, running, failed, blocked, total, usd, elapsed_secs, branch, base_short }`
- `LogRow { text: String, severity: LogSeverity }` (`Info|Ok|Warn|Error`)
- `RunSummary { run_id, dir: PathBuf, status: RunStatus, goal, done, total }`

### Control channel (for run-control features)

`ControlCommand` = `AbortRun | AbortTask{id} | RetryTask{id} | Approve | Veto`,
`#[serde(tag="cmd", rename_all="snake_case")]`. Write with
`control::append(run_dir, &cmd)`; the orchestrator's `control_watchdog` tails
`<run-dir>/control.jsonl`. In the TUI, `WatchUi::send_control(cmd, note)`
appends to `current_dir()` and sets `toast`.

---

## 2. Conventions (follow these when implementing)

- **Verify each chunk**: `cargo build -p <crate>`, then
  `cargo test -p <crate> <filter>`, then `cargo clippy -p <crate>`. All must be
  clean before committing.
- **One commit per feature**, imperative subject, e.g.
  `feat(cli): auto-dismiss control-action toast`.
- **rustfmt discipline**: the repo was fully formatted in PR #13. Keep new code
  fmt-clean. Do **not** run crate-wide `cargo fmt` and sweep unrelated files
  into a feature commit — format only your hunks (or run `cargo fmt` then
  `git add -p` / revert unrelated files).
- **TUI test harness**: tests live in the `#[cfg(test)] mod tests` at the
  bottom of `pilot_watch_tui.rs`. Use the existing helpers:
  `summary(id, status)`, `sample_model()`, and
  `render_to_string(&mut ui, w, h) -> String` (renders via ratatui
  `TestBackend`, returns screen text). `UNI`/`ASC` are `Glyphs` consts.
  `tempfile` is a dev-dependency of `wingman-cli`.
- **ASCII safety**: every glyph the UI can emit must have an ASCII variant via
  `Glyphs::pick`. The test `ascii_glyphs_avoid_non_ascii_codepoints` fails if a
  new glyph leaks non-ASCII in ascii mode — extend it.
- **Branching**: cut feature branches off `main`; one PR per bundle.

---

## 3. Features

### Tier A — quick wins (TUI-only, low risk)

#### A1. Auto-dismiss the toast
**Problem:** `WatchUi::toast` (set by `send_control`) stays on the footer until
the run is switched — it reads as stuck.
**Plan:**
- Add `toast_since: Option<u64>` (the `frame` when set) to `WatchUi`; set it in
  `send_control` alongside `toast`.
- In `run_loop` (after `ui.frame = …`) or in `draw`, clear `toast`/`toast_since`
  once `frame - toast_since` exceeds ~25 frames (~3 s at the 120 ms cap).
- `render_footer` already prioritises the toast; it just becomes `None` on
  expiry.
**Test:** a pure helper `toast_expired(now, since) -> bool`; assert it clears
after the threshold and persists before it.
**Effort:** ~30 min.

#### A2. Empty-state guidance
**Problem:** With no runs, `run_loop` returns exit 1 silently and the "loading
run…" path is unhelpful; `pilot status` just errors.
**Plan:**
- In `pilot.rs::watch`, when `list_runs` is empty and stdout is a terminal,
  print a friendly block: *"No pilot runs yet. Start one with:
  `wingman pilot run \"<goal>\"`"* and return 0.
- In `draw`, when `model` is `None` but `runs` is non-empty keep "loading run…";
  when a run has zero tasks, render "plan pending…" in the Tasks pane.
- Mirror the empty message in `pilot.rs::status`.
**Test:** factor the message into a `const`/fn and assert its content.
**Effort:** ~30 min.

#### A3. Glyph legend in help
**Problem:** Newcomers don't know `◇ ‼ ⊘ ↻ ○ ·`.
**Plan:** In `render_help`, append a "Legend" section mapping each task/agent/
run glyph to its meaning, themed through `Glyphs`. Pull glyphs from the same
`task_status_style` / `agent_status_style` / `run_status_glyph` used elsewhere.
**Test:** extend `help_overlay_lists_keys_and_wins_over_detail` to assert the
legend header renders.
**Effort:** ~30 min.

#### A4. `NO_COLOR` support
**Problem:** No respect for the `NO_COLOR` convention.
**Plan:**
- Add `color: bool` to `WatchUi`. Resolve once in `pilot.rs::watch`:
  `let color = std::env::var_os("NO_COLOR").is_none();` and thread into
  `run(...)` next to `ascii`.
- Add `fn tint(color: bool, style: Style, c: Color) -> Style` returning `style`
  unchanged when `!color`; route `fg(...)` sites through it. Simplest scoped
  version: when `!color`, substitute `Color::Reset` in the style helpers.
**Test:** render with `color=false`; assert no color styles set on the
`TestBackend` cells.
**Effort:** ~1–2 h (touch points spread across style helpers).

#### A5. Task-status filter (mirror the log filter)
**Problem:** Long task lists can't be narrowed; the log has a severity filter
but tasks don't.
**Plan:**
- Add `TaskFilter { All, Active, Failed }` with `.next()` / `.accepts(
  TaskStatus)` / `.label()`, analogous to `SevFilter`.
- Add `task_filter: TaskFilter` to `WatchUi`; bind `t` to cycle it.
- In `render_tasks`, filter before rendering; show the filter + shown/total in
  the pane title. Clamp `tasks_sel` against the *filtered* length and map the
  selected filtered row back to the real `TaskRow` for `Enter`/detail.
**Test:** unit-test `TaskFilter::{next, accepts}`; render a mixed model filtered
to `Failed` and assert only failed rows appear.
**Effort:** ~1–2 h (selection↔filter index mapping is the fiddly part).

---

### Tier B — medium (new surface, still contained)

#### B1. Copy-to-clipboard (`y` yank)
**Value:** Grab the selected task id / run id / PR URL without mousing.
**Plan:** add `arboard = "3"` to `wingman-cli`; bind `y` to copy a
context-sensitive value (Tasks → task id; Runs → run id; prefer a PR URL if
present); toast "copied t3"; guard headless failures.
**Test:** factor `yank_target(&WatchUi) -> Option<String>` (pure) and test it.
**Effort:** ~1–2 h.

#### B2. Agent pane selectable + agent detail overlay
**Plan:** extend `Focus` with `Agents`; add `agents_sel`/`agents_len`, include
in `cycle_focus` + arrow routing + `HitAreas`. Generalise `detail` into
`Detail { Task(String), Agent(String) }` and render an agent overlay (name,
role, status, tool, pid, uptime, usd + that agent's recent log lines).
**Test:** `agent_detail_overlay_shows_worker` render test; focus-cycle test.
**Effort:** ~2–3 h.

#### B3. Search match navigation (`n`/`N`) + inline highlight
**Plan:** keep the query, highlight matches inline in `log_line` (split on the
case-insensitive match, style the matched span), and move `log.scroll` to the
next/prev matching line with `n`/`N` (disable `follow`).
**Test:** `match_line_indices(rows, query)` + `highlight_spans(text, query)`.
**Effort:** ~2 h.

#### B4. Responsive small-terminal layout
**Plan:** in `draw`, if `area.width < ~70 || area.height < ~14`, render a
single-column, sidebar-less, meters-less layout (or a "terminal too small —
resize to ≥70×14" panel). Hide the sidebar below a width threshold.
**Test:** `render_to_string(&mut ui, 40, 10)` must not panic and shows the
too-small message; normal size still renders the grid.
**Effort:** ~1–2 h.

#### B5. Abort/retry a task from the detail overlay
**Plan:** while a task `detail` is open, bind `x` → confirm `AbortTask{id}` and
`r` → confirm `RetryTask{id}` for that task (reuse `Confirm` + `send_control`).
Note the keys in the overlay footer.
**Test:** open detail, assert the `Confirm.cmd` carries the right command.
**Effort:** ~45 min.

#### B6. `.gitattributes` for line endings
**Plan:** repo-root `.gitattributes`:
```
* text=auto eol=lf
*.rs text eol=lf
*.md text eol=lf
```
Optionally `git add --renormalize .` in a separate commit (large diff).
**Effort:** ~15 min.

---

### Tier C — bigger / product-level

#### C1. `pilot ls` — list runs
**Plan:** new `PilotAction::Ls { --json }`; handler calls
`dashboard::list_runs(&project.root)` and prints a table (run id, status,
done/total, spend, elapsed, branch) or JSON. Reuse `RunSummary`; `load_state`
per run for spend/elapsed.
**Test:** the table formatter is a pure fn over `Vec<RunSummary>`.
**Effort:** ~1–2 h.

#### C2. Real desktop notifications
**Plan:** add `notify-rust` (opt-in via `--notify` / `WINGMAN_NOTIFY`); fire
alongside `WatchUi::note_bell_events`. Best-effort; never fail the run.
**Effort:** ~1–2 h (cross-platform testing is the cost).

#### C3. Confirm the control action *landed*
**Problem:** the toast confirms the command was *written*, not that the
orchestrator *acted*.
**Plan:** after `send_control`, remember the expected transition (run→Aborted,
task→Failed/reassigned). On the next `reload`, if state matches, update the
toast to "aborted ✓"; if not within N seconds, "no response — is the run live?".
Closes the feedback loop and detects a dead orchestrator.
**Test:** a pure `note_control_result(prev, next)` decision.
**Effort:** ~2–3 h.

#### C4. Pause / resume a run (**touches the orchestrator**)
**Plan:** new `ControlCommand::Pause`/`Resume`; add a `paused` flag in
`run_actor` (mirror the existing `aborting` flag) that makes `AssignTask` a
no-op while set without failing the run. CLI `pilot pause/resume`, TUI key `p`.
Own PR, adversarial review, tests in `orchestrator.rs` (mirror PR #12).
**Effort:** ~half day.

#### C5. Run replay / timeline scrubber
**Plan:** the event log is append-only; add `pilot replay <run>` that loads all
events and lets `←/→` step a cursor, rebuilding the model via
`dashboard::build_model` over a truncated event slice. Largest item; design as
its own doc.
**Effort:** multi-day.

---

## 4. Suggested sequencing

1. **UX-polish bundle (Tier A)** — A1 + A2 + A3 + A4 + A5 in one PR (TUI-only,
   independently testable, no orchestrator risk). Highest friendliness-per-line.
2. **Interaction bundle** — B1 (clipboard) + B2 (agent detail) + B5 (task abort
   from detail).
3. **Polish/infra** — B3 (search nav), B4 (responsive), B6 (.gitattributes).
4. **Product** — C1 (`pilot ls`), C2 (notifications), C3 (action confirmation).
5. **Orchestrator-touching** — C4 (pause/resume), then C5 (replay); each its own
   carefully-reviewed PR.

## 5. Gotchas learned while building the current UI

- The orchestrator is an **in-process actor**; `pilot watch` is a **separate
  process**. Cross-process control only works through `control.jsonl` — don't
  call the orchestrator directly from the TUI.
- Aborting a run naively deadlocks against the **retry watchdog** (it turns
  every `Failed` into a `Reassign`). The `aborting` flag in `run_actor` is what
  lets `drive_to_completion` converge — any new "stop the run" feature (pause,
  etc.) must account for that pump.
- The watchdog derives the run dir from `run_dir(project_root, run_id)`. The
  orchestrator test `cfg()` helper hardcodes `run_id = "test-run"` — control-file
  tests must create the run dir with that same id.
- `--ascii` and `NO_COLOR` are orthogonal: ascii swaps *glyphs*, color swaps
  *styles*. Keep them independent.
- Selection panes (Tasks, and Agents once added) need their `*_sel` clamped on
  every `reload` because tasks/agents come and go.
