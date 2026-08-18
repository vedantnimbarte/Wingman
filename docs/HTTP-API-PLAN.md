# HTTP API — implementation plan

Companion to [HTTP-API.md](HTTP-API.md) (the spec). This is the build order:
what each phase adds, which files it touches, what it reuses, and what proves it
works. Branch: `feat/http-api`. Every phase is one commit that builds green and
passes `cargo test` + `cargo clippy -- -D warnings`.

## Decisions taken (and why)

| Decision | Chosen | Rationale |
|---|---|---|
| Transport | HTTP/1.1 on `tokio::net::TcpListener`, SSE for streams | Zero new deps. `wingman-autonomous::webhook` already has the request parser and `pilot intake slack` already runs this exact shape async. |
| Where the code lives | `crates/wingman-cli/src/serve/` | The routes need `runtime::`, `commands::`, and the binary path. A separate crate could not depend on `wingman-cli` without inverting the dependency graph. |
| Turn execution | Child process `wingman --print --json` per turn | `runtime::build_*` and every CLI command resolve the project from `std::env::current_dir()`, which is process-wide — concurrent turns in different repos would race it. Pilot already spawns workers exactly this way, the NDJSON event stream maps onto SSE line-for-line, and a panicking turn cannot take the daemon down. |
| Pilot read/control | In-process filesystem access | `dashboard::list_runs / load_state / tail_events` and `control::append` are pure functions over explicit paths. Nothing to spawn. |
| Long-tail CLI coverage | Declarative route table plus `/v1/exec` | Forty hand-written handlers that each shell out to one subcommand is forty copies of the same twelve lines. One table gives per-route param validation *and* generates `/v1/schema`. |
| Auth | Single bearer token, constant-time compare | Chosen over HMAC: every client (curl, Shortcuts, a Slack bot) can send a header; not every client can implement request signing. |
| Project scoping | Server-side allowlist | A stolen token reaches only the repos listed in the global config, never arbitrary filesystem paths. |

## Phase 1 — server core

**Goal:** `wingman serve` binds, authenticates, resolves projects, and answers
the meta routes. Nothing repo-mutating yet.

New: `crates/wingman-cli/src/serve/mod.rs`, `http.rs`, `auth.rs`, `projects.rs`,
`sse.rs`, `resp.rs`.
Changed: `cli.rs` (`Serve` subcommand + `--addr`/`--init-token`/`--list`/
`--allow-yolo`), `main.rs` (module), `wingman-config/src/lib.rs`
(`ServeConfig`, `ServeProject`, `PushConfig`; `${ENV}`/keyring resolution for
`token`; **not** added to `PROJECT_SAFE_KEYS`).

- `http.rs`: accept loop, request parse (reusing
  `wingman_autonomous::webhook::header_boundary_and_len`), method/path/query
  split, 1 MiB body cap, `Connection: close`, per-connection `tokio::spawn`,
  response and SSE writers.
- `auth.rs`: bearer extraction, `subtle::ConstantTimeEq` compare, the startup
  refusals from the spec.
- `projects.rs`: id/root resolution, duplicate-id and missing-root rejection,
  `contains_path` guard for body-supplied paths.

Routes: `GET /v1/health`, `GET /v1/projects`, `GET /v1/schema`.

**Proof:** unit tests for request parsing (split reads, missing
`Content-Length`, oversized body), auth (wrong token, missing header, correct
token), and project resolution (unknown id, duplicate id, path escape). One
integration test binds `127.0.0.1:0` and drives `/v1/health` plus a 401.

## Phase 2 — pilot read and control

**Goal:** run a pilot fleet from a phone.

New: `serve/routes/pilot.rs`.
Changed: nothing outside `serve/` — `dashboard::*` and `control::append` are
already public.

- Reads map onto `list_runs`, `load_state`, `tail_events`, and
  `render_dashboard(...).to_ascii_width(w)` for the text dashboard.
- Control appends the matching `ControlCommand`. Returns `409` when the run is
  not in a state that command applies to (approve on a run that is not
  `AwaitingApproval`), which the API checks against the loaded `RunState` —
  `control.jsonl` itself is lenient by design and would silently ignore it.
- `/stream` tails `tasks.jsonl` from a byte offset on a 500 ms poll, mirroring
  `ControlReader`. In-process `broadcast` is not usable here: the orchestrator
  lives in a different process.
- `POST /pilot/runs` spawns `wingman pilot run --yes …` detached, then waits for
  the run directory to appear (bounded, ~5 s) so it can return the real run id
  rather than a promise.
- `POST /pilot/goals` writes an intake file via the same helper
  `pilot_intake.rs` uses.

**Proof:** tests over a temp run directory — seed `tasks.jsonl`/`state.json`,
assert the summary JSON, assert `approve` on a non-gated run is `409`, assert an
appended command lands in `control.jsonl` verbatim.

## Phase 3 — sessions and turns

**Goal:** hold a conversation with the agent over HTTP; close the laptop, resume
from a phone.

New: `serve/routes/sessions.rs`, `serve/child.rs` (spawn + NDJSON → SSE).
Changed: `cli.rs` and `commands/headless.rs` — add `--resume <session-id>`, which
loads the transcript with `wingman_session::load_session` +
`records_to_messages` and builds the loop with `AgentLoop::with_history`, and
make `--session-id` name the log for `--print` (today headless always calls
`SessionLog::create`). This is a genuine CLI gap: `wingman --print --resume` is
useful with no server involved.

- A turn spawns `wingman --print --json --mode <effective> --session-id <id>
  [--resume]` with `.current_dir(root)`, streams stdout lines, translates each
  `AgentEvent` to one SSE event, and emits a final `stop`.
- Effective mode is `min(requested, ceiling)`; a request above the ceiling is
  `403`.
- `max_concurrent_turns` is a `tokio::sync::Semaphore`; per-session in-flight
  state is a `Mutex<HashMap<SessionId, ()>>` so a second turn on one session is
  `409`. Client disconnect kills the child.

**Proof:** a fake-child test (a script echoing known NDJSON) asserting the
NDJSON → SSE translation, ceiling clamping (`--mode yolo` under an `auto-edit`
ceiling is 403), and that a second concurrent turn on one session is 409.

## Phase 4 — reads, admin, and passthrough

**Goal:** the whole CLI reachable, with validation on the routes that matter.

New: `serve/routes/table.rs` (the declarative table + dispatcher),
`serve/routes/exec.rs`, `serve/argv.rs` (subcommand allowlist, `--mode`
clamping, refusal list).

- The table is `&[Route { method, path, subcommand, params }]`; the dispatcher
  builds argv from validated params only. `/v1/schema` serialises the same
  table, so documentation cannot drift from behaviour.
- `/v1/exec` shares `argv.rs` with the table so one allowlist governs both.
- `GET/PATCH /v1/config` reads the merged config and writes only the global
  file, refusing any patch that touches `[serve]`.

**Proof:** tests for argv construction (a param that is not in the route is
dropped, not appended), every refusal in the allowlist, `--mode` clamping in
both `exec` and table routes, and that a patch touching `[serve]` is rejected.

## Phase 5 — push, remote client, docs

**Goal:** the server reaches out; the CLI reaches in.

New: `serve/push.rs`, `crates/wingman-cli/src/remote.rs`.
Changed: `cli.rs` (`--remote`, `WINGMAN_REMOTE`), `commands/pilot_watch_tui.rs`
(a state source that polls the API instead of `state.json`), `docs/CLI.md`,
`docs/CONFIGURATION.md`, `docs/INDEX.md`, `README.md`.

- `push.rs` watches the same run directories `/v1/events` does and POSTs on
  subscribed transitions, reusing the Slack-shaped payload from
  `wingman_autonomous::notify`.
- `remote.rs`: `--print` streams SSE to stdout; `pilot watch` swaps its polling
  source; everything else routes through `/v1/exec` and prints stdout verbatim.

**Proof:** a loopback test that the push payload is emitted once per transition
(not once per poll), and a round-trip test running a real `wingman serve` on
`127.0.0.1:0` driven by `--remote --print`.

## Risks

| Risk | Mitigation |
|---|---|
| Turn subprocess startup cost (embedder init) on every turn | Same cost `--print` already pays; `indexd` keeps the index warm. If it hurts, a persistent per-project child is a later optimisation behind the same routes. |
| `/v1/exec` is remote code execution at a yolo ceiling | Argv array (no shell), subcommand allowlist, ceiling clamp, and `--allow-yolo` required at launch. |
| SSE clients behind proxies that buffer | 15 s `:keepalive` comments; `Cache-Control: no-cache`, `X-Accel-Buffering: no`. |
| Two servers on one repo racing pilot control | `control.jsonl` appends are atomic and idempotent per command; run creation is guarded by the run-id directory. |
| Scope drift into a web UI | Explicit non-goal in the spec. |
