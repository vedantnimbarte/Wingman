# Web UI (the panel)

A React control panel served by `wingman serve` and embedded in the binary.
The board, pilot runs, sessions, config, and observability — from a browser, on
this machine or a phone on the same network.

```bash
wingman serve                      # the panel is at http://<[serve].addr>/
```

Status: **phases 0–2 shipped** — the delivery pipeline, the app shell, sign-in,
the live event stream, and the board. Runs, config, sessions and insights
arrive in phases 3–6; the panel names the phase on each section rather than
pretending they are missing by accident. Build order and the reasoning behind
each decision are in [WEB-UI-PLAN.md](WEB-UI-PLAN.md).

The terminal board (`wingman board`) is unaffected and stays the default. This
is a second renderer, not a replacement — see [BOARD.md](BOARD.md).

---

## How it reaches you

The panel lives in `ui/` and is built by npm, not cargo. `crates/wingman-cli/
build.rs` stages exactly three files — `index.html`, `app.js`, `app.css` — into
`OUT_DIR`, and `serve::ui` embeds them with `include_bytes!`. There is no
runtime file dependency: `wingman serve` remains a single static binary.

Vite is configured to emit those three stable names rather than hashed ones
(`ui/vite.config.ts`). Hashed filenames would force a dependency that can walk
an unknown tree, and cache-busting buys nothing here — the `ETag` is a hash of
the bytes, so a rebuilt bundle invalidates the cache and an unchanged one gets
a `304`.

**A missing bundle does not break the build.** When `ui/dist` is absent —
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
npm run build          # tsc --noEmit && vite build → ui/dist
cd .. && cargo build --release
```

For UI work, skip the Rust rebuild entirely:

```bash
wingman serve          # in one terminal
cd ui && npm run dev   # in another — HMR, proxies /v1 to 127.0.0.1:8787
```

Change the proxy target in `ui/vite.config.ts` if your `[serve].addr` differs.

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

## Scope

The panel is single-user and single-machine, like the board it renders. It is
not multi-tenant, has no accounts, and does not sync. Cards still move because
runs move — dragging a card would force a task transition past the dep gates
and the write-set conflict check, which is the machinery that makes runs
converge. See [BOARD-PLAN.md](BOARD-PLAN.md) § Scope creep toward drag-and-drop.
