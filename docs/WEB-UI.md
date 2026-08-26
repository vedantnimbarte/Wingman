# Web UI (the panel)

A React control panel served by `wingman serve` and embedded in the binary.
The board, pilot runs, sessions, config, and observability — from a browser, on
this machine or a phone on the same network.

```bash
wingman serve                      # the panel is at http://<[serve].addr>/
```

Status: **shipped.** Board, live pilot runs, sessions and chat, the full config
surface, and cost/context observability. Build order and the reasoning behind
each decision are in [WEB-UI-PLAN.md](WEB-UI-PLAN.md).

The terminal board (`wingman board`) is unaffected and stays the default. This
is a second renderer, not a replacement — see [BOARD.md](BOARD.md).

---

## How it reaches you

The panel lives in `panel/` and is built by npm, not cargo. `crates/wingman-cli/
build.rs` stages exactly three files — `index.html`, `app.js`, `app.css` — into
`OUT_DIR`, and `serve::ui` embeds them with `include_bytes!`. There is no
runtime file dependency: `wingman serve` remains a single static binary.

Vite is configured to emit those three stable names rather than hashed ones
(`panel/vite.config.ts`). Hashed filenames would force a dependency that can walk
an unknown tree, and cache-busting buys nothing here — the `ETag` is a hash of
the bytes, so a rebuilt bundle invalidates the cache and an unchanged one gets
a `304`.

**A missing bundle does not break the build.** When `panel/dist` is absent —
a fresh clone, a contributor without node, `cargo install wingman` — `build.rs`
embeds a placeholder page saying the UI was not built, and everything else
compiles and runs normally. The API is unaffected either way.

The cost of that convenience is a real failure mode: a broken UI build would
otherwise ship a binary that serves the placeholder and passes every other
check. Two things catch it — `wingman serve` prints which one it is at startup,
and CI's `web-ui` job runs the `#[ignore]`d `ui_bundle_is_embedded` test after
building the bundle.

```
wingman serve: listening on 127.0.0.1:8787 — 1 project(s), ceiling auto-edit, auth OFF (loopback)
  wingman          /home/you/code/wingman
  panel            http://127.0.0.1:8787/
```

## Building it

```bash
cd ui
npm ci
npm run build          # tsc --noEmit && vite build → panel/dist
cd .. && cargo build --release
```

For UI work, skip the Rust rebuild entirely:

```bash
wingman serve          # in one terminal
cd ui && npm run dev   # in another — HMR, proxies /v1 to 127.0.0.1:8787
```

Change the proxy target in `panel/vite.config.ts` if your `[serve].addr` differs.

## Authentication

The static shell — `/`, `/app.js`, `/app.css` — is served **without** a token.
A browser has to load the page before it can present a credential, so gating it
would be a chicken-and-egg with no upside: those three files contain no project
data, no config, and no run state. Everything the panel *shows* comes from
`/v1`, which is authenticated exactly as [HTTP-API.md](HTTP-API.md) describes.

Unknown paths fall back to `index.html` so client-side routes deep-link. That
fallback stops at `/v1`: an unknown API path stays a JSON `404` rather than
becoming a `200` with an HTML body, which is the failure mode that makes a
client's error handling silently wrong.

### Signing in

`GET /v1/health` reports `auth_required`. On a loopback daemon with no token it
is `false` and the panel goes straight to the shell — there is no secret to
demand. Otherwise the panel asks for the token once and posts it to
`POST /v1/ui/session`, which verifies it with the same constant-time comparison
every other route uses and returns it as a cookie:

```
Set-Cookie: wingman_token=<token>; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000
```

**`HttpOnly`** is the point: the panel has an npm dependency tree, and a token
readable by page script is one bad transitive dependency away from leaving the
machine. **`SameSite=Strict`** stands in for CSRF tokens — no cross-site
request carries this cookie, and no CORS headers are set, so another origin can
neither send it nor read the reply.

**`Secure` is deliberately absent.** It would be correct over TLS and wrong
here: the panel is reached over plain HTTP on loopback or a LAN address — the
phone-on-the-sofa case it exists for — and a `Secure` cookie on those origins is
simply discarded. The threat it defends against already sees
`Authorization: Bearer` on every other request to the same daemon.

The cookie carries the token itself rather than a session id, so there is no
session table and no expiry bookkeeping. What that costs: a leaked cookie is a
leaked token, and the only revocation is `wingman serve --init-token` to rotate
it.

`DELETE /v1/ui/session` signs out and is never gated — a browser holding a
cookie the server has stopped accepting must still be able to drop it.

An explicit `Authorization` or `X-Wingman-Token` header always wins over the
cookie, so a script or CI job that sends a credential never has a stale browser
cookie substituted for the one it just supplied.

> **Why this matters beyond exfiltration:** `EventSource` cannot set request
> headers. With a bearer token the panel would have had to put it in the query
> string of `/v1/events` — and therefore into every access log. The cookie
> rides along on its own.

## The board

Five columns, expandable cards, sub-rows, and a task detail panel — the same
board `wingman board` renders, in a browser.

**Nothing about a card's state is computed in the browser.** The column, the
roll-up and the badges are derived server-side by `wingman-board`, the same
code the TUI calls. A second derivation in TypeScript would be the first thing
to disagree with the terminal on a Friday afternoon.

The board refreshes on the `/v1/events` stream rather than polling: a run
transition anywhere means some card's derived column may have moved.

Adding a card and dispatching one both work from the panel. Dispatch spawns
`wingman pilot run --detached` through the same `dispatch_card` the CLI uses —
including the fix for the bug where `Command::output()` blocked for the entire
run because the detached grandchild held the pipes.

**A card can only be dispatched into a repo this daemon serves.** The board
registry is global and remembers every repo pilot has ever run in;
`[[serve.projects]]` is narrower. Dispatching outside it is a `403` — otherwise
the API token would start agents with write access in directories the
allowlist never named.

On first start the daemon registers its allowlisted repos on the board, once,
so the panel opens onto a board that can actually take a card. It is guarded by
a stored flag, so projects you deliberately forget stay forgotten.

### There is no drag-and-drop

Not because it is hard in React. Moving a card means forcing a task transition
past the dependency gates and the write-set conflict check, which is the
machinery that makes runs converge. If it is ever built it belongs in the
orchestrator behind its own gate — see
[BOARD-PLAN.md](BOARD-PLAN.md) § Scope creep toward drag-and-drop. Cards move
because runs move.

## Runs

Every pilot run in the selected repo, and a detail view per run: status, spend,
tokens, integration branch, the plan, and the workers.

The plan is **indented by dependency depth**, deepest last, with each task
naming what it waits on. Expanding a task shows its goal, model, worktree,
declared writes, transcript id and the worker's reported outcome.

The detail view holds an `EventSource` on the run's `tasks.jsonl` stream and
re-reads the `state.json` snapshot on each event, rather than applying events
to local state. `state.json` is written atomically after every event, so this
stays authoritative — a second reducer in the browser would be one more thing
to keep in step with the orchestrator.

### The plan gate

A run with `[pilot.approval]` configured stops after planning and waits. That
is the moment the panel earns itself: the whole plan is on screen, and
**Approve plan** / **Reject plan** are one click.

Per-task **Retry** and **Abort task** appear only where the server would accept
them — retry for a task that stopped without finishing, abort for one still
moving — because a button that always returns `409` is worse than no button.

**A control action is recorded, not applied.** Each one appends a single
command to the run's `control.jsonl`; the orchestrator's watchdog picks it up
on its own schedule, and the API never reaches into the running process. The
panel says so ("Sent `approve` — the run applies it on its next check") rather
than optimistically flipping the status to something that has not happened yet.

### Elapsed time

Reported only where it means something: exact when a task recorded an
`ended_at`, counting up while the run is live, and omitted for a task with no
recorded end on a run that has already finished. In that last case the task
never stopped cleanly, and counting from now would report the time since the
run died rather than any work done.

## Config

Every setting Wingman has, as forms **generated from the config types
themselves**. `GET /v1/config/schema` derives a JSON Schema from the
`wingman-config` structs, so each field arrives with its `///` comment as help
text and its real default. Add a field to a Rust struct and it appears in the
panel, documented, with nobody editing the UI.

Booleans, numbers, strings, string lists and enums each get a proper control —
an enum's per-variant doc comment becomes its option tooltip. Shapes a form
cannot express are edited as JSON: arrays of objects like
`[[hooks.pre_tool_use]]`, and maps keyed by names you choose like
`[mcp.<name>]`. Flattening those into text inputs would drop fields on save.

### Four rules the panel follows

**Saves land in the global file, never a repo's.** The path is printed above
the form. A project's `.wingman/config.toml` is the untrusted layer, and an API
that could write it would be a way to smuggle executable keys into a repo.

**`[serve]` is shown, read-only, with the reason.** `PATCH` refuses it outright
— a server that can rewrite its own token, ceiling or project allowlist has no
ceiling. Hiding the section instead would just turn that into a mysterious
failed save.

**Credentials render as `set · hidden`, not as empty boxes.** Reads come back
redacted; an empty input would offer to overwrite a real key with nothing.
Replacing one is a deliberate act behind a **Replace** button, and only a value
you actually type is ever sent.

**There is no client-side validation.** `PATCH` round-trips the result through
the real config parser and returns its error, which the panel shows inline. One
validator, and it is the one that actually has to load the file.

### Saves are a minimal diff

A save edits the TOML document in place rather than re-serialising it, so
changing one field produces a one-line diff. Your comments, key order and
formatting survive, and a comment sitting above a setting stays attached to it
when the value changes.

## Sessions

Transcripts, and holding a conversation with the agent.

A session is not a server object with a timeout — it is the same
`.wingman/sessions/<id>.jsonl` the TUI writes. One started in the browser shows
up in `wingman session list` and resumes from a terminal, because the file on
disk *is* the state. That is what makes "start on the laptop, continue on the
phone" work with no sync protocol behind it.

Each turn streams as it happens: text, the model's reasoning (folded away by
default — it is the working-out, not the answer), each tool call with its input
and output, and the verification gate's verdict. `EventSource` cannot be used
here, because a turn is a `POST` with a body; the panel reads the stream off
`fetch` directly.

When a turn finishes the panel re-reads the transcript rather than keeping what
it assembled while streaming, so a just-finished turn renders identically to
every older one.

**A second turn on a session already running one is refused.** The child would
otherwise replay a transcript the first turn is still appending to, and the two
would interleave into one incoherent history. The panel says so rather than
showing a bare `409`.

**Deleting reports what happened to the search index.** A finished turn is
embedded into the global session store for `recall_session`, so removing only
the JSONL would leave the conversation findable by search — a delete that does
not delete. The response says whether the index entry went too, and the panel
repeats it.

## Insights

**What this repo has cost, and what the same work would have cost elsewhere.**
Your real token volume repriced against ten other models, as a bar chart with
your actual spend in it. It is a price comparison, not a recommendation — a
cheaper model that needs three attempts is not cheaper.

Below it, the per-turn tax: the system prompt and tool schemas every turn pays
for before you type anything, with a per-tool breakdown of where the schema
budget goes.

Then the long tail — `knows`, `doctor`, `attest`, `diff`, `explain`, `review`,
`router stats`, `index status`, `trust`, `memory`, `config` — listed from
`GET /v1/schema`, which is generated from the server's own route table. A
report added to the CLI appears here without the panel changing. Output that
parses as JSON renders as JSON; anything else renders as the text it is,
because that is exactly what those routes promise.

There is no charting library. Two bar lists are a CSS grid and a percentage
width, and a library would have brought its own colour opinions into a palette
whose rule is that hue means epistemic status. Cost is never coloured — the row
you actually paid for is marked by weight, not by turning it green.

## Scope

The panel is single-user and single-machine, like the board it renders. It is
not multi-tenant, has no accounts, and does not sync. Cards still move because
runs move — dragging a card would force a task transition past the dep gates
and the write-set conflict check, which is the machinery that makes runs
converge. See [BOARD-PLAN.md](BOARD-PLAN.md) § Scope creep toward drag-and-drop.
