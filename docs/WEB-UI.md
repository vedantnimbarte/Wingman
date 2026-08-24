# Web UI (the panel)

A React control panel served by `wingman serve` and embedded in the binary.
The board, pilot runs, sessions, config, and observability — from a browser, on
this machine or a phone on the same network.

```bash
wingman serve                      # the panel is at http://<[serve].addr>/
```

Status: **phase 0 shipped** — the delivery pipeline (build, embed, serve,
deep-link, release). The panel currently renders daemon liveness and nothing
else. Build order and the reasoning behind each decision are in
[WEB-UI-PLAN.md](WEB-UI-PLAN.md).

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

> **Phase 1 will change how the browser holds the token.** It trades it for an
> `HttpOnly; SameSite=Strict` cookie via `POST /v1/ui/session`, so the token is
> never readable by page script. Until then the panel reads nothing but
> `/v1/health`, which needs no token at all.

## Scope

The panel is single-user and single-machine, like the board it renders. It is
not multi-tenant, has no accounts, and does not sync. Cards still move because
runs move — dragging a card would force a task transition past the dep gates
and the write-set conflict check, which is the machinery that makes runs
converge. See [BOARD-PLAN.md](BOARD-PLAN.md) § Scope creep toward drag-and-drop.
