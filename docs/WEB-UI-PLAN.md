# Web UI (`wingman serve` → browser) — build plan

A React control panel served by the existing daemon: the kanban board, live
pilot runs, sessions, observability, and the full config surface. The TUI stays
exactly as it is — this is a second renderer, not a replacement.

> **Status: phase 0 shipped; phases 1–6 planned.** Phases ship in order and
> each one is independently useful. User-facing docs are in
> [WEB-UI.md](WEB-UI.md).

---

## Why this is smaller than it sounds

Three things are already true, and they carry most of the weight:

**`wingman-board` is headless.** `view.rs` opens with *"Headless board layout…
Nothing here needs a terminal"*, and *"The ratatui half in `wingman-cli` only
draws what this produces."* Columns, cards, sub-rows and roll-ups are plain
structs. The web board is a third consumer of `BoardView`, alongside
`to_ascii_width` and the ratatui layer — so the two boards derive state from the
same code and **cannot disagree**, which is the same guarantee the board already
has with `pilot watch`.

**`serve` already exposes most of the panel.** Shipped today: pilot read and
control, sessions and streaming turns, `/v1/events` firehose, `GET`/`PATCH
/v1/config` (redacted, validated), and the table-driven long tail — `cost`,
`context`, `knows`, `doctor`, `attest`, `diff`, `explain`, `review`, `router
stats`, `index`, `memory`, `trust`, `checkpoint`, `rewind`, `schedule`. Phases 3
and 6 below add **zero** new Rust.

**npm is already in CI.** [ci.yml:155](../.github/workflows/ci.yml:155) runs
`setup-node@v4` + `npm install` + `npm run build` for the VS Code extension, in
"its own npm toolchain, separate from the Rust build." React is a second
consumer of a toolchain that already runs, not a new ecosystem.

What genuinely does not exist: static-file serving, board routes, config schema
emission, and the UI itself.

---

## Decisions taken

| Decision | Choice | Consequence |
| --- | --- | --- |
| TUI's fate | **Kept, unchanged** | `board_tui.rs` (1,718 lines) is not touched. Board over SSH keeps working. |
| Framework | **React + TypeScript + Vite** | New npm project at `ui/`. Chosen for future extension, per the brief. |
| Bundle delivery | **Embedded at compile time** | `wingman serve` stays a single static binary with no runtime file dependency. |
| Config UI | **Server emits a schema; UI generates forms** | New config fields appear in the UI for free. No hand-maintained form that can drift from the Rust structs. |
| v1 scope | **Full control panel** | Six phases. Board, runs, config, sessions, observability. |
| Visual direction | **Fully web-native** | No terminal pastiche. Design system in § Design below. |

---

## Two decisions that needed an owner

Both were cheap to reverse before Phase 0 and expensive after. **Both are now
decided: (a) in each case.**

### 1. Embedding must not break `cargo build` ✅ (a)

`include_bytes!("../../ui/dist/app.js")` is a compile error when `dist/` does
not exist — which is the state of every fresh clone, every contributor without
node, and `cargo install wingman`. Embedding makes the npm build **load-bearing
for the Rust build**, and today the node job is explicitly *"Informational: a
separate ecosystem shouldn't block Rust PRs."* That contract changes here, and
it should change deliberately.

- **(a) `build.rs` stubs the missing files.** ~15 lines: if `ui/dist/` is
  absent, emit a placeholder `index.html` reading "web UI not built — see
  docs/WEB-UI.md" into `OUT_DIR` and embed that instead. `cargo build` never
  breaks, `cargo install` works, release CI builds the real bundle.
  **Recommended.** The cost is that a broken UI build ships a working binary
  with a stub page, so CI must assert the real bundle is present on release.
- **(b) Commit `ui/dist/` to the repo.** Cargo builds standalone with no
  build.rs. Cost: a minified bundle in every diff and a merge conflict surface.
- **(c) Cargo feature `web-ui`, default off.** Purest, but the panel is then
  absent from the binary people actually download, which defeats the point.

### 2. How the browser holds the token ✅ (a)

Every route except `/v1/health` requires the token. The UI is same-origin
(served by the daemon), so the browser must present it somehow.

- **(a) Trade the token for an `HttpOnly; SameSite=Strict; Secure` cookie**, via
  a new `POST /v1/ui/session`. The token never becomes readable by page script,
  so a UI bug or a malicious dependency in the React tree cannot exfiltrate it.
  ~30 lines in `auth.rs`. **Recommended** — this UI will grow a dependency tree,
  and `serve`'s existing posture (ceiling a request cannot raise, `[serve]`
  refused over the API, credentials redacted on read) is too careful to hand
  that up now.
- **(b) `sessionStorage` + an `Authorization` header.** Zero server change. The
  token is readable by any script on the page, forever.

Note the token authorizes running arbitrary allowlisted subcommands in your
repos via `POST /v1/projects/{p}/exec`. It is not a read credential.

---

## Phase 0 — delivery pipeline ✅ shipped

Prove the whole path end to end on the smallest possible surface, before any
design or feature work rests on it.

- `ui/` — Vite + React + TS. **Stable output filenames**
  (`entryFileNames: 'app.js'`, `assetFileNames: 'app.[ext]'`), so embedding is
  three `include_bytes!` calls and needs **no new Rust dependency**. Hashed
  filenames would force `include_dir` or `rust-embed`; cache-busting is
  meaningless for a locally-served single-user binary, and the build version
  already gives us an `ETag`.
- `crates/wingman-cli/src/serve/ui.rs` — serve `GET /` → `index.html`,
  `/app.js`, `/app.css`, correct MIME types, `ETag` from the build version, and
  an SPA fallback so client-side routes deep-link. Mounted **before** the auth
  gate for the shell only; every `/v1/*` call it makes stays authenticated.
- `build.rs` per decision 1.
- CI: extend the existing node job to build `ui/` and type-check it.
  `release.yml` gains a node step before `cargo build` for all five targets.
- **Ships:** a page that renders `GET /v1/health`. Worthless as a feature,
  decisive as proof — build, embed, serve, auth, deep-link, five-target release.

### Found while building it

**`write_raw` grew an `extra` headers parameter rather than a sibling.** Static
assets need `ETag` and `Cache-Control`, and no existing writer took headers. A
second writer in `ui.rs` would have duplicated the half-close at the bottom of
`write_raw` — the one that stops Windows sending an RST that clients read as
"connection forcibly closed". One writer, one place that subtlety lives.

**`reason()` had no `304` arm,** so the first conditional request answered
`HTTP/1.1 304 Status`. Pre-existing and invisible until a route returned a
status the table never anticipated.

**Rerun detection is by mtime, and I misdiagnosed it once.** Moving `ui/dist`
away and back left the embed stale, and the first explanation — that cargo
does not normalise the `..` in `crates/wingman-cli/../../ui/dist` — was wrong.
Cargo compares timestamps, and `mv` preserves them, so the restored files were
older than the last build-script run and correctly did not trigger one. A real
`npm run build` rewrites all three files and does. The path was cleaned up
anyway for legibility, but it fixed nothing; the honest note is in `build.rs`.

**`is_shell` takes `(method, segments)` rather than a `Request`,** because
`Request::segments` is private and a test-only constructor would have been more
machinery than the decision it tests.

**The placeholder needs a gate, not just a comment.** A binary built without
`ui/dist` serves a "not built" page and passes every other check, which is
precisely the kind of silent degradation that ships. `ui_bundle_is_embedded` is
`#[ignore]`d so `cargo test` stays green without node, and CI runs it with
`--ignored` after building the bundle.

## Phase 1 — design system and shell

- Tokens, type scale, and layout from § Design, as CSS custom properties.
- App shell: project switcher (`GET /v1/projects`), nav, light/dark.
- Auth flow per decision 2.
- One `EventSource` on `/v1/events` in a context provider — the firehose is
  already "the same detector outbound push uses, so the stream and a webhook
  cannot disagree." Every later phase subscribes rather than polling.
- Error, empty and loading states defined **once**, here. Six phases of
  features will otherwise each invent their own.

New routes, all three deliberately outside the auth gate:

| Method | Path | Why ungated |
| --- | --- | --- |
| `POST` | `/v1/ui/session` | It *is* the authentication — it checks the token itself, constant-time, before setting anything. |
| `DELETE` | `/v1/ui/session` | A browser holding a cookie the server no longer accepts must still be able to drop it. |
| `GET` | `/v1/health` (extended) | Gained `auth_required`, so the panel can skip its sign-in screen on a loopback server with no token instead of demanding a secret that does not exist. |

## Phase 2 — the board

The only phase needing new board plumbing. The board is **global** (spans
projects, lives in `~/.wingman/board.db`), but `table.rs`'s routes are
project-scoped *without exception* — so these are hand-written in `dispatch()`
alongside `/v1/config`, not table entries.

New `crates/wingman-cli/src/serve/board.rs`, calling `wingman-board`
**in-process** (already a dependency of `wingman-cli`) rather than shelling out
to `board list --json`:

| Method | Path | Backing |
| --- | --- | --- |
| `GET` | `/v1/board` | `column::derive` + `rollup` — the same derivation the TUI renders |
| `POST` | `/v1/board/cards` | `card::add` |
| `POST` | `/v1/board/cards/{id}/dispatch` | `dispatch::spawn` |
| `POST` | `/v1/board/cards/{id}/archive` | `card::archive` |
| `DELETE` | `/v1/board/cards/{id}` | `card::rm` |
| `GET` | `/v1/board/projects` | `registry::list` |

In-process matters: `board dispatch` already had a bug where `Command::output()`
blocked for the entire run because the detached grandchild held the pipes
([BOARD-PLAN.md:275](BOARD-PLAN.md:275)). Reusing the fixed `dispatch::spawn`
inherits the fix; a fresh shell-out would re-earn the bug.

React: five columns, expandable cards, sub-rows, detail panel, add and dispatch.

**Drag-and-drop is explicitly out**, and not because it's hard in React.
[BOARD-PLAN.md:265](BOARD-PLAN.md:265): forcing a task transition overrides dep
gates and the write-set conflict check, "the machinery that makes runs
converge… If it is ever built, it belongs in the orchestrator with its own gate,
not in the renderer." A web renderer is still a renderer. Cards move because
runs move.

## Phase 3 — pilot runs, live

**Zero new Rust.** Run list, run detail, task DAG with `dep` edges, per-task
agent/model/cost, live `tasks.jsonl` via the existing
`/pilot/runs/{run}/stream` SSE, and approve / veto / abort / retry against the
shipped control routes.

The plan gate is the moment this phase justifies itself: `run.awaiting_approval`
arrives on the firehose, and approving a seven-task plan is genuinely better
with a mouse and the full plan on screen.

## Phase 4 — config

The largest phase, and the one with a real dependency question.

**Schema derivation.** `wingman-config` is 2,888 lines, 28 structs, 166 fields —
with good `///` doc comments throughout (`reasoning` alone carries a five-line
explanation of how it maps onto three vendors' parameters). Those comments are
the difference between a usable settings UI and a wall of unlabelled inputs.

- **`schemars` + `#[derive(JsonSchema)]`** lifts `///` into `description`
  automatically. One new dependency (MIT, pure Rust, clears `deny.toml`), and a
  derive on 28 structs in a crate everything depends on. **Recommended.**
- **Zero-dep fallback:** `serde_json::to_value(Config::default())` yields every
  field name, nesting and default for free — but no descriptions, and no type
  for any field that defaults to `None`. Cheaper, meaningfully worse.

Then `GET /v1/config/schema`, and React generates the forms from it. Rules the
UI must enforce, all of them already enforced server-side:

- `[serve]` is **not offered** — `PATCH` refuses it outright ("a server that can
  rewrite its own token, ceiling, or allowlist has no ceiling"). The UI should
  render it read-only with the reason, not hide it and let the request 400.
- Redacted values render as redacted, never as empty inputs that would `PATCH`
  a credential away.
- `PATCH` validates by round-tripping through the real parser; its `400` message
  is the error text shown inline. The UI does no validation of its own — one
  validator, and it's the one that actually loads the config.
- Writes hit the **global** file only. The UI must say so, because
  `.wingman/config.toml` in a repo is the untrusted layer and silently not
  writing there would be the worst kind of surprise.

## Phase 5 — sessions and chat

Session list, transcript rendering, and a streaming turn against
`POST /v1/sessions/{id}/turns` — `text`, `tool_start`, `tool_result`, `usage`,
`verification`, `stop`. Transcripts are normal
`.wingman/sessions/<id>.jsonl`, so a session started in the browser shows up in
`wingman session list` like any other.

Largest UI surface in the plan. Two contracts to respect: a second turn on the
same session returns `409` while the first is in flight, and `DELETE` reports
`deindexed` so a partial delete is visible rather than a surprise later.

## Phase 6 — observability

Mostly generated from `GET /v1/schema`, which publishes the route table and
"cannot drift from the implementation… they are the same array."

Hand-built where the shape earns it: **cost** (`?compare` repricing your real
token volume against other models is the README's headline claim and deserves a
real chart), **context** (the per-turn tax), **doctor** (a checklist), and
**index status**. Everything else — `knows`, `attest`, `router stats`, `diff`,
`explain`, `review` — renders generically from the table, JSON where it parses
and text where it doesn't, which is what the routes already promise.

## Design

**Direction: Ledger.**

Wingman's pitch is that it *proves* things. It asks the language server instead
of grepping, it runs your build and your tests before it may say "done", and it
tells you the exact token cost of every turn — the README opens by pointing out
that almost no other agent will. A panel for it should be an accounting surface
for machine work, not a dashboard.

So the system runs on one rule, and the rule comes from the product:

> **Colour encodes epistemic status, and nothing else.** Proven, asserted,
> failed. Never a brand accent, never a chart's decoration, never a hover state.

That is what makes a scan of the board answer the only question that matters:
what here is actually true, and what is merely running.

Cost, deliberately, gets **no colour** — it gets typography. Every row carries a
right-aligned tabular figure, and nothing else in the layout is right-aligned.

**Signature — the ledger column.** A single continuous right-hand rule runs the
full height of every view. Sub-task spend sums into its card, cards sum into
their column header, columns sum into the global header, and the number is
present at every level of nesting, always, never truncated, never behind a
click. The eye tracks one vertical line from a task's $0.41 to the fleet's
total. It is the one thing in the panel you would remember, and it is the one
thing Wingman brags about.

**Palette.** Cool neutral paper, not cream; true dark, not near-black with an
acid accent.

| Token | Light | Dark | Used for |
| --- | --- | --- | --- |
| `--paper` | `#FCFCFD` | `#0E1014` | Ground |
| `--ink` | `#16181D` | `#E8EAF0` | Primary text, all figures |
| `--muted` | `#5C6370` | `#8A909C` | Labels, metadata |
| `--rule` | `#E4E6EB` | `#22262E` | Hairlines, the ledger column |
| `--proven` | `#0B7A5A` | `#3FB68B` | Verification passed, task done |
| `--asserted` | `#A85F0B` | `#E0A458` | Running, unproven, awaiting approval |
| `--failed` | `#B03A2E` | `#E0705F` | Failed, blocked, vetoed |

Measured against `--paper`, every status clears WCAG AA for normal text —
light: 5.19 / 4.76 / 5.87, dark: 7.50 / 8.72 / 6.03. Light `--asserted` was
`#B4690E` when this palette was first written and measured 4.12:1; it was
darkened in Phase 1 rather than shipped, because status text nobody can read is
not status.

`--failed` is brick rather than alarm red: a failed task inside a bounded-retry
system is information, not an emergency, and the verification gate already
retries before it gives up.

**Type.** IBM Plex Sans for prose and UI, IBM Plex Mono for every figure, id,
path and glyph — one superfamily, drawn for machine-readout contexts, with real
tabular figures so the ledger column aligns. The display role is earned through
weight, size and tracking rather than a third face.

This deliberately avoids Inter + JetBrains Mono, which is what a developer tool
defaults to, and it avoids the near-black-plus-acid-green dashboard, which is
what an AI-designed developer tool defaults to. One superfamily, one accent
rule, everything else quiet.

**Layout.**

```
┌────────────────────────────────────────────────────────────┬─────────┐
│ wingman   [project ▾]                                      │  $14.82 │
├──────────┬──────────┬──────────┬──────────┬────────────────┼─────────┤
│ BACKLOG  │ PLANNED  │ RUNNING  │ REVIEW   │ DONE           │         │
│    $0.00 │    $0.00 │    $1.24 │    $0.88 │          $12.70│         │
├──────────┼──────────┼──────────┼──────────┼────────────────┤         │
│ Fix LSP  │ Add board│ ▾Refactor│ Bench    │ Windows job    │         │
│ restart  │ 7 tasks  │  RAG 3/7 │ runner   │ 4/4      $2.10 │   ▲     │
│          │          │ ├ T1 impl│ 6/6 $0.88│                │   │     │
│          │          │ │ ●$0.41 │          │                │  sums   │
│          │          │ ├ T2 test│          │                │ upward  │
│          │          │ │ ●$0.83 │          │                │         │
│          │          │ └ T3 docs│          │                │         │
│          │          │   dep T2 │          │                │         │
└──────────┴──────────┴──────────┴──────────┴────────────────┴─────────┘
   glyph colour = epistemic status      figures = tabular, right-aligned
```

**Floor, not announced:** responsive to mobile (phone access is a stated goal),
visible keyboard focus, `prefers-reduced-motion` respected, and the colour rule
never load-bearing on its own — every status carries a glyph as well, because
three hues that all mean "state" is exactly where a colour-blind user loses the
board.

---

## Risks

**Scope.** Six phases is a quarter of evenings, not a weekend. Each ships
standing alone, so stopping after Phase 2 leaves a working board rather than a
half-panel — but the full control panel is the stated target and it is large.

**The npm build becomes load-bearing.** Today the node job is informational.
After Phase 0 the binary contains the bundle, so a failed UI build must fail the
release. Decision 1 keeps `cargo build` working for contributors without node;
it does not remove the new coupling, it only makes it graceful.

**Two boards, one derivation — keep it that way.** The value of Phase 2 calling
`wingman-board` in-process is that web and TUI cannot disagree. The moment a
convenience field gets computed in TypeScript instead of in `rollup.rs`, that
guarantee is gone and the drift will be found by a user, not a test.

**Config UI ambition vs. the schema.** Generated forms are only as good as the
schema. Enums typed as `String` (`reasoning`, `permission_mode`, `tui.theme`)
render as free-text boxes unless they are given real enum types or the schema
teaches them their variants. Worth fixing in the Rust, where it also helps the
CLI, rather than special-casing in React.

**The panel is a bigger blast radius than the CLI.** A browser tab holding a
credential that can run subcommands in your repos is a different exposure from a
terminal that already has your shell. Decision 2 is the mitigation and should
not be deferred to "after it works".

---

## Out of scope, on purpose

- Drag-and-drop card movement — orchestrator work behind its own gate, per
  [BOARD-PLAN.md:265](BOARD-PLAN.md:265).
- Multi-user, accounts, or team sharing. Single-user, single-machine.
- Replacing or deprecating any TUI surface.
- `/v1/skills`, `/v1/mcp`, `/v1/providers` — needs CLI commands first.
- Editing `[serve]` from the UI it configures.
