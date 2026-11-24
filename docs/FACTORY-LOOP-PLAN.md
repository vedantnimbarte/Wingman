# Factory loop: implementation plan

Four additions that close the loop from "production is unhappy" to "a
reviewed fix is waiting": telemetry intake, a reviewer that remembers, a
per-task QA gate with card lint, and a UI polish loop.

Ideas come from Eric Tech's
[super-board](https://github.com/EricTechPro/super-board) (MIT), a Claude Code
skill pack that runs Build → QA → Review lanes over a GitHub Projects board.
This plan takes the ideas that fit Wingman. It doesn't copy the skill pack's
text or its GitHub-centred storage.

> **Status: phase 1 built** (on `feat/factory-loop`, not yet merged), with
> phase 1b deferred until 1a shows noise. Phases 2–4 are planned. Each phase
> ships on its own. Deviations are recorded at the end of each phase.

---

## Decisions taken (and why)

| Decision | Alternative rejected | Why |
| --- | --- | --- |
| Intake files **board cards**, never dispatches | Telemetry candidates go to the daemon queue or auto-run | An error count is not a spec. A person promotes the card. Telemetry candidates are `TrustLevel::Known`, so `decide()` never returns AutoRun for them anyway. |
| The board stays **local** | Mirror cards and lane logs to GitHub Projects or issue comments, as super-board does | Wingman's board is local by design ([BOARD-PLAN.md](BOARD-PLAN.md)). Intake reads GitHub, as the daemon already does, and writes nowhere outside `board.db`. |
| A **content fingerprint** dedupes intake | The daemon's existing `source + title` seen-set | Sentry retitles issues and PostHog groups drift. `sentry:<id>` does not. The fingerprint is stored as a card label (`fp:sentry:4411`), so no schema change is needed. |
| **Deterministic** checks before any LLM verifier | One LLM verifier per candidate (super-collect) | "Still happening" and "duplicate" are facts the source API and the board already answer. An LLM verifier for "already fixed" is phase 1b, and only if 1a files noise. |
| Reviewer memory is an **event in `tasks.jsonl`** | An in-memory `failure_history`, as today | `RunStore` is already the append-only truth and survives restarts. The in-memory history dies with the process, and the next reviewer never sees it. |
| QA is an **inline call**, like the reviewer | A spawned tester worker per task | Acceptance checks already *run* (E3). What's missing is judgement: is each acceptance criterion asserted by a test at the right layer? One call answers that, and a worker per task doubles spawn cost. |
| UI loop runs **in the current checkout on a new branch** | A worktree with its own dev server (super-board) | The user's dev server with hot reload already serves the checkout. A worktree needs a second server, a free port and copied env files. The cost: it needs a clean tree. |
| The command is **`wingman polish`** | `wingman refine` | `refine` is already J1 goal refinement (`wingman-autonomous/src/refine.rs`). |

---

## Phase 1: telemetry intake

New pilot daemon sources: `sentry`, `posthog`, and `custom`. Each one files
cards in the project's board Backlog.

### Facts this builds on

- Sources are free functions in `wingman-autonomous/src/daemon.rs`, wired
  by name in `run_cycle` (:492). To register one, add a branch there, add
  it to `IMPLEMENTED_SOURCES` (`wingman-cli/src/commands/pilot.rs:2331`),
  and update the default-sources comment (`wingman-config/src/lib.rs:3203`).
- `Candidate` (daemon.rs:18) has no body, URL or fingerprint. Its `title`
  becomes the goal.
- Fetch errors are dropped silently (`if let Ok(found)`), so an unreachable
  source looks the same as a quiet one.
- `wingman-autonomous` has no HTTP client. Every source shells out to
  `gh`/`git` through `CommandRunner` (pr.rs:121).
- No daemon path touches `wingman-board`.

### Changes

1. **`Candidate` gains two fields:** `fingerprint: Option<String>` and
   `body: Option<String>` (evidence as markdown: links, counts, first and
   last seen, release). Existing sources leave both `None`.
2. **Fetching is split into an impure fetch and a pure mapping.** Each
   source has `fn candidates_from_<source>(json: &Value, cfg) ->
   Vec<Candidate>`, unit-tested against fixture JSON. The fetch is one
   `reqwest` GET or POST (reqwest 0.13 is already in the workspace). The
   token comes from config with the existing `${ENV_VAR}` placeholder,
   never a literal.
3. **Sources report their coverage.** A source returns
   `Read(n) | Empty | Unavailable(reason)`, and the daemon logs one line
   per source per cycle. Unreachable is never reported as empty.
4. **Sentry** (`[pilot.daemon.sentry] org, project, token, window_days = 14`):
   list unresolved issues seen inside the window. One issue becomes one
   candidate, with fingerprint `sentry:<id>`, a body holding the issue
   link, event count, user count, first and last seen, and release, and the
   goal `Fix <culprit>: <title>`.
5. **PostHog** (`[pilot.daemon.posthog] host, project_id, token,
   min_users = 5, failure_events = [...]`): HogQL queries for four signals.
   | Signal | Files when | Fingerprint |
   | --- | --- | --- |
   | `exception` (grouped by issue id) | at least `min_users` people | `posthog:exception:<id>` |
   | configured failure event | at least `min_users` people | `posthog:event:<name>` |
   | `rageclick` (URL plus element) | at least `min_users` people | `posthog:rage:<url>#<el>` |
   | web vitals p75 per path | at least 50 samples, and LCP > 4 s, INP > 500 ms or CLS > 0.25 | `posthog:vitals:<path>` |

   Dead clicks are left out because they have a high false-positive rate.
   Add them when someone asks.
6. **Custom** (`[[pilot.daemon.custom]] name, kind = "http" | "cli",
   target, token?, items = "/data", id = "/id", title = "/title"`): fields
   are JSON pointers (`serde_json::Value::pointer`), so there is no jq
   dependency. `cli` runs through `CommandRunner` and parses stdout. The
   fingerprint is `custom:<name>:<id>`.
7. **Filing.** In the daemon loop (pilot.rs, around :2435), a candidate
   that has a fingerprint skips the queue and goes through
   `file_intake_card`:
   - It looks up a card in this project carrying the label
     `fp:<fingerprint>`.
   - **Open hit:** it appends a dated "Seen again" line to the card's notes.
     No new card.
   - **Archived hit:** it files a new card that names the old one. That's a
     recurrence.
   - **No hit:** it files a new card with labels `intake`,
     `intake:<source>` and `fp:<fingerprint>`.
   - `--dry-run` prints what it would file, as `pilot daemon --dry-run`
     already does.

### 1b (only if 1a files noise): "already fixed" verifier

One fast-model call per new candidate, made through `[router].fast_model`.
It gets the evidence and `git log --since=<first_seen>` subjects, and
returns `file | drop-fixed | needs-triage`. An unclear result is never a
drop: it files with the label `needs-triage`.

### Tests

- Fixture JSON → candidates for each source.
- Fingerprint dedup: open hit, archived hit, no hit, each against an
  in-memory `board.db`.
- `Unavailable` is surfaced, not swallowed.

### As built (deviations from the plan above)

- **`Candidate` is unchanged.** Telemetry never goes through
  score and decide, so it got its own `telemetry::Finding` instead of two
  optional fields on every existing source's struct. The flow is: mappers in
  `wingman-autonomous/src/telemetry.rs`, fetch in
  `wingman-cli/src/commands/pilot_telemetry.rs`, dedup and filing in
  `wingman-board/src/intake.rs`. It runs from the daemon loop before
  `run_cycle`, and file-change wakes skip it.
- **Rage clicks are grouped by page path, not path plus element.** The
  element chain is too long and too unstable to use as a key. Session replay
  finds the element.
- **Archived cards refile only on evidence.** A recurrence needs the
  source's `last_seen` to be later than the archive time. Without that rule,
  archiving a card was undone on the next poll.
- **Coverage is per source.** `Read(n)` or `Unavailable(why)`. An empty
  read is just `read 0`.

---

## Phase 2: reviewer that remembers

### Facts this builds on

- The per-task reviewer is `review_task_inline` (pipeline.rs:1700), which
  has its own hardcoded `SYSTEM` string. `prompts/reviewer.md` is only
  used for planner-created reviewer *tasks*.
- Inputs come in this order: task, goal, **worker summary**, then the
  diff. The summary comes before the diff, so the reviewer reads the
  worker's framing first. Acceptance results are not passed.
- Rework notes become a `TaskStatus{Failed}` event plus an in-memory
  `RetryState.failure_history` (orchestrator.rs:419), which is not
  persisted. The next reviewer round sees none of the prior findings.
- Rework is capped by the retry ladder (`max_retries_per_task`, default 3).

### Changes

1. **Reorder the input and ask for hypotheses first.** The order becomes:
   goal, acceptance criteria and their results, the diff, then the worker
   summary under the heading "Claims to check, not evidence". The output
   JSON gets a `hypotheses: [string]` field, listed first. *Ceiling:* in a
   single call the model has still read the summary. Splitting it into two
   calls is possible if blind review measurably helps; it isn't worth it
   up front.
2. **Each finding gets a `class`:** `gap | bug | verification_miss |
   scope_drift | over_engineering`. `over_engineering` never triggers
   rework on its own. It shows up in the report and the PR body, but
   `next_status` skips it.
3. **Persisted rounds.** A new `Event::TaskReview { task, round, report }`
   is appended in `handle_finalize`. Each finding gets a stable id
   (`R1`, `R2`, …) that carries across rounds.
4. **The memory pass.** When a task has a prior `TaskReview`, the
   reviewer prompt includes those findings. The model must mark each one
   `fixed | not_fixed | no_longer_applies` and cite `file:line` for every
   `fixed`. If any `not_fixed` finding is at or above the gate, the task is
   sent back for rework. Settled findings are not raised again.
5. `pilot watch` task detail and `pilot export` show the review rounds.

### Tests

- `parse_review` handles the new fields and accepts old JSON.
- `next_status` with over-engineering only: approve.
- A prior finding marked `not_fixed` at or above the gate: rework.
- `TaskReview` events replay through `apply()`.

---

## Phase 3: QA gate and `board lint`

### Facts this builds on

- `tester` is only a role the planner can choose
  (`manager-planner.md:61`). It is not a gate.
- The gap between a worker reaching Review (worker.rs:~650) and the
  reviewer running (`handle_finalize`, orchestrator.rs:2365) is the natural
  place for a QA gate.
- Cards have no acceptance-criteria field. `Card::prompt()` returns the goal.

### Changes: QA gate

1. **Add `qa_task_inline`** next to `review_task_inline`. It is one call on
   `[pilot].qa_model` (falling back to `reviewer_model`). It gets the
   goal, the acceptance specs and results, and the full diff including test
   files. It returns:
   - a **ledger** with one row per acceptance criterion: `{criterion, layer:
     unit|integration|e2e|none, evidence: "file:line" | null}`
   - **gaps**, each with a severity.
2. **It runs in `handle_finalize` before the reviewer.** A gap at or above
   `[pilot.pr].qa_rework_severity` (default `high`) sends the task back
   through the same Failed and retry path the reviewer uses, and the
   reviewer is skipped for that round. The ledger is persisted as an
   `Event::TaskQa`.
3. **Gating:** a new capability `per_task_qa`. It is **off by default**
   until measured, then on for copilot if it pays for itself.

### Changes: `wingman board lint`

1. **`wingman board lint [<card>] [--json] [--write]`.** It adds a
   `BoardAction::Lint` variant (cli.rs:545) with its handler in
   `commands/board.rs`.
2. **One fast-model call per Backlog card.** It returns `{verdict:
   clear|vague, questions: [..], acceptance: [..]}`. A vague card prints
   its questions.
3. **`--write`** appends the suggested criteria to the card's goal under
   `Acceptance criteria:`. The planner already turns goal text into
   `PlannedTask.acceptance`, and `critique_plan` already flags empty
   acceptance, so that sharpens what reaches the planner.
4. Without a provider it fails with a clear message. There is no regex
   fallback.

### Tests

- QA ledger parsing.
- `handle_finalize`: a QA bounce skips the reviewer.
- Lint verdict parsing.
- `--write` is idempotent: running it twice adds no duplicate section.

---

## Phase 4: `wingman polish`

`wingman polish <url> [--scope <path>...] [--rounds 3] [--pr] "<brief>"`
is a bounded critique-and-fix loop on one page. It needs the `browser`
build feature, and without it the command says so and exits.

### Facts this builds on

- `wingman-browser` has `capture(url, timeout)` and `Session`. Neither one
  sets a viewport or color scheme. Both use `LaunchOptions::default_builder()`.
- The `Message` contract carries `ContentBlock::Image`, so a checker can be
  given screenshots.

### Changes

1. **`wingman-browser`:** `capture_with(url, CaptureOpts { width, height,
   dark, full_page })`. It sets the window size through `LaunchOptions` and
   `prefers-color-scheme` through CDP `Emulation.setEmulatedMedia`.
2. **Setup:** it refuses a dirty tree, creates branch `polish/<slug>`, and
   takes round-0 shots at 1440 and 390 px, light and dark. Shots go to
   `.wingman/polish/<slug>/` and are never committed.
3. **Each round:**
   - **Checker:** one vision call with the shots, the brief and a built-in
     rubric (design /20: specificity, hierarchy, typography, color,
     composition; audit /20: accessibility, performance, responsive,
     theming, integrity). It returns at most 8 ranked, typed problems
     (P0 to P2) and a score /40. Each round gets a fresh call; the only
     thing carried forward is a one-line-per-round ledger.
   - **Fixer:** a fresh single-agent run with edit tools, restricted to
     `--scope`. It gets the problems and runs the verification gate. Green
     means commit `polish(<slug>): round N`. Red means `git reset --hard`
     to the round's start.
   - **Reshoot.**
4. **Stop rules** (whichever comes first): N rounds; two checks in a row
   with no P0 or P1; two fix rounds in a row reverted.
5. **Finish:** the report is written to `.wingman/polish/<slug>/report.md`
   with the score line, before and after shot paths, and the problems fixed.
   `--pr` opens a draft PR through the existing PR path.

### Tests

- Stop-rule state machine (pure).
- Checker output parsing.
- `CaptureOpts` reaching `LaunchOptions` is covered by a compile check; a
  live capture is behind the feature, like `BrowserGate`.

---

## Not doing (and why)

- **GitHub Projects or issue mirror:** the board stays local (decision above).
- **Difficulty-graded model router:** `[router.classes]` and learned
  routing (#250) already cover this ground. Revisit after #250 merges.
- **Guard hooks** (secret-file reads, deletes outside the project): mostly
  covered by containment, the denylist and credential scrub. One gap is
  noted: reading a `.env` *inside* the project is allowed. That is a
  separate one-line denylist change if wanted.
- **The `prs` intake source** (recurring review findings across merged
  PRs): useful, but it builds on phase 2's persisted findings. Add it after
  phase 2.

## Observed in passing

Pilot workers don't use `build_turn_gate`. They get only
`[pilot].turn_gate_cmd` (`commands/worker.rs:168-187`), so the affected-tests
and LSP-diagnostics gates from the README's "proves the work" claim don't
apply inside pilot runs. That is outside this plan, but worth an issue.
