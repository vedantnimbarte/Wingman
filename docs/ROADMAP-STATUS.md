# Roadmap Status

Tracks the differentiation roadmap. Everything below is implemented on `main` (or
an open PR), compiles, is clippy-clean, and has unit tests for its logic. Items
whose *runtime* needs external infrastructure (a browser binary, a mail system,
Slack, a hosted server) are noted — the code is complete and tested; only the
live end-to-end run needs that infrastructure.

**Read "implemented and tested" as exactly that, and not as "wired into a
default run."** Several entries are opt-in (a cargo feature, a config switch)
or are logic cores whose production call site is still pending. Where that is
true it is called out in the row.

The tested-but-uncalled modules `wingman-autonomous` used to carry are gone:
[#105](https://github.com/vedantnimbarte/Wingman/issues/105) and
[#129](https://github.com/vedantnimbarte/Wingman/issues/129) are closed, the
four parked modules and the never-called webhook receiver were deleted, and the
crate's test count dropped from 514 to 478 as a result. That drop is the
feature — the number now tracks code that runs.

## Shipped

| Item | What | Runtime needs |
|---|---|---|
| MSRV honesty (L1) | Declared floor set to 1.88; gate re-enabled | — |
| LSP code-actions (T1.1) | `lsp_code_action`; client applies `workspace/applyEdit` | a language server |
| Wingman-as-MCP-server (T1.2) | `wingman mcp-serve` (tools + memory resources) | — |
| HTTP/SSE API | `wingman serve` (pilot control, streaming turns, CLI passthrough) + `--remote` | — |
| **Agent Client Protocol** | `wingman acp` over stdio: turn loop, plus `session/request_permission` (the editor can decline a single tool call) and `fs/read_text_file` (reads come from the unsaved buffer). Both are an *additional* gate — containment is checked before the client is asked, so a client can narrow what the agent touches, never widen it. **`fs/write_text_file` is not wired** ([#127](https://github.com/vedantnimbarte/Wingman/issues/127)): routing writes through the client takes them out of the dispatch path that writes `/undo` checkpoints and the audit log. | an ACP client (Zed, JetBrains, Neovim, Emacs) |
| **Mid-run pilot steering** | `pilot tell "<msg>"` injects into a live worker's next turn; `pilot ask` waits for its reply. Routed through the run's `control.jsonl` like every other cross-process control. | a running pilot session |
| **Portable reasoning control** | `reasoning = off\|low\|medium\|high` (config, `WINGMAN_REASONING`, `--reasoning`, `/reasoning`) mapped onto Anthropic `thinking.budget_tokens`, OpenAI `reasoning_effort`, and Gemini `thinkingConfig`. `ContentBlock::Thinking` carries signed reasoning back through history so Anthropic multi-turn tool use keeps working; reasoning streams to the UI dimmed and to stderr in `--print`. Off by default. Backends without a reasoning control ignore it and `wingman doctor` names them. | a provider with reasoning |
| **Concurrent read dispatch** | A turn whose tool calls are all on `parallel_safe_tools` (and on none of `mutating_tools`) dispatches them together; any other name serialises the whole batch. Results keep the model's order. MCP tools are deliberately excluded — a stdio server that mishandles concurrent requests is not reproducible from here. | — |
| Git-native auto-commit (T1.3) | `[git].auto_commit` | a git repo |
| Local-first preset (T3.7) | `wingman router preset local` + `local` class | a local model |
| Explain-and-teach (T3.8) | `wingman explain` | a provider |
| Benchmark harness (L5) | `wingman bench` | a provider |
| Affected-tests receipt (L3) | Edited symbols surfaced in the gate receipt | — |
| Agent SDK (T2.5) | `docs/SDK.md`; embed core or drive over MCP | — |
| Audit trail (T3.9) | `[audit].enabled` JSONL compliance log | — |
| **reqwest 0.13 unify (L2)** | All first-party crates on reqwest 0.13 + ring; only `hf-hub` (embeddings) keeps a transitive 0.12 | — |
| **Windows shell containment** | `run_shell` children go into a Job Object: no orphaned trees on timeout, no clipboard or cross-process handle access, capped process count. **It does not scope the filesystem** — `Availability::scopes_filesystem()` is false there, so `shell_sandbox = "required"` still refuses on Windows rather than accept a weaker guarantee, and `wingman doctor` prints what the mechanism misses ([#124](https://github.com/vedantnimbarte/Wingman/issues/124)). | — |
| **Browser verification (T2.4)** | `wingman-browser` crate + `BrowserGate` (`[verify].browser`); screenshot diff vs baseline. **Not in the default build, and the gate fails open when no browser is present.** | a Chrome binary + `--features browser` |
| **Server-backed team memory (T3.9)** | `[team].endpoint` + `wingman memory push` / `pull` (non-clobbering merge) | a team memory HTTP endpoint |
| **Pilot Slack/email intake (L4)** | `wingman pilot intake slack\|email` → intake files | Slack app / mail delivery |
| **Editor bridge (T2.6)** | `editors/vscode` — VS Code extension over `wingman mcp-serve` | npm build + VS Code |

## In open PRs (not merged)

As of 2026-09-15, seven area PRs are open against `main` (0.4.0). Nothing in this
table is on `main` yet. Each branch reports `cargo fmt`, `clippy -D warnings`, and
its crates' tests clean locally (Windows). On GitHub, the per-OS test jobs pass on
#247–#252, but the required-jobs gate is red on all six because the `audit` job fails (on #247
and #252 it is `cargo audit` flagging RUSTSEC-2026-0285), which needs triage (fix or a reasoned `deny.toml` ignore) before
anything merges; #253's CI was still running at the time of writing, with coverage red.

| PR | Item | What | Not validated live |
|---|---|---|---|
| [#247](https://github.com/vedantnimbarte/Wingman/pull/247) | PR review loop | `wingman review <pr> --comment [--dry-run]` posts findings as one inline GitHub review, deduped only against reviews the gh user posted. Pilot daemon source `pr_reviews` (off by default) reworks trusted reviewers' unresolved threads on pilot's own PRs, capped by `max_review_rounds` and `[pilot].max_usd`, disabling armed auto-merge before it pushes. | no real PR, no gh calls, no provider-backed rework; threads/comments past 100 not fetched |
| [#248](https://github.com/vedantnimbarte/Wingman/pull/248) | Tree-sitter expansion | C++, Java, Kotlin parsing (symbols, chunks, outline, `edit_symbol`); incremental reparse via a `TreeCache` on re-chunk; theme-aware TUI highlighting, `NO_COLOR`, read-only file view (`v` in the sidebar). | TUI not checked by eye; live watcher not run; cargo-deny not run locally; Windows only |
| [#249](https://github.com/vedantnimbarte/Wingman/pull/249) | LSP symbol analysis | `who_calls` answers from a language server first; affected tests narrowed from edited symbols to referencing tests; memory staleness resolves named symbols. | no real language server (fake in-process server only) |
| [#250](https://github.com/vedantnimbarte/Wingman/pull/250) | Single-agent roadmap | Learned routing + durable PR verdicts ([#183](https://github.com/vedantnimbarte/Wingman/issues/183)); detached `indexd start/stop/status` and import-ranked prefetch; budgeted search escalation. | gh post-merge checks, learned routing in a real session, Unix liveness code (never compiled locally) |
| [#251](https://github.com/vedantnimbarte/Wingman/pull/251) | Insights / export / rewind | `wingman metrics` (time to first output, tokens per completed task, verified-done rate, routing outcomes) across CLI, `knows`, HTTP, panel, `bench --markdown`; redacted `session export` / `pilot export` / `/export`; turn-grouped rewind timeline with preview and restore. | no live model timings, no running `wingman serve` or browser, no real pilot run |
| [#252](https://github.com/vedantnimbarte/Wingman/pull/252) | Pilot hardening | JSON-schema HTTP acceptance; R6 gitleaks / cargo-audit / lockfile license scan / PR comment; J15 escalations fired during the run with test-count baselines and force-push detection; adaptive concurrency, speculative pre-spawn, turn rollback; merge-fixer workers and a knowledge-keeper; hard E11 gate, plan-time critic, true first-try; golden-reference eval judge, weekly `eval.yml`, daemon feedback polling. | gitleaks, cargo-audit, `gh pr comment`, live force-push, pre-spawn, and every provider-backed path; `eval/baseline.jsonl` is not committed, so the weekly workflow cannot gate yet |
| [#253](https://github.com/vedantnimbarte/Wingman/pull/253) | Pilot capabilities | J12 skill-pack index, `ssh-keygen` signatures, transitive deps; J11 container and Firecracker vm tiers with patch-back; J7 tool synthesis (`propose_tool` + approval gate); J13 `pilot daemon --watch` + shell-free git hooks; `pilot validate-providers` matrix. | no Docker daemon or KVM host (neither tier has ever run a worker); Unix-only sandbox code never compiled locally; no real provider keys; no git-hosted pack index |

## Notes on the infra-dependent items

- **Browser verification** — the screenshot-diff logic (`wingman_browser::diff_ratio`)
  is pure and unit-tested; `capture()` drives headless Chrome behind the
  `chrome` feature (compile-verified). Build the CLI with `--features browser`
  and configure `[verify.browser] url = "…"` + a `baseline` PNG. Fail-open when
  no browser is present.
- **Team memory server** — `push`/`pull` speak a trivial HTTP contract
  (`POST /memory` a JSON pack, `GET /memory` returns one); pack collection and
  the non-clobbering merge are tested. Point `[team].endpoint` at any service
  implementing that contract (a ~20-line handler).
- **Pilot intake** — Slack event parsing, `.eml` parsing, and intake-file
  writing are unit-tested; the Slack front end is a minimal HTTP server (put
  TLS/ingress in front), and email ingests `.eml` files your mail system
  delivers.
- **Editor bridge** — complete TypeScript extension (thin MCP client). Ships via
  the VS Code Marketplace on its own npm toolchain, separate from the Rust
  release pipeline; `npm install && npm run build` in `editors/vscode`.
- **ACP** — the turn loop, permission requests, and buffer reads are covered by
  unit tests including a fake client that declines a call and one that serves a
  buffer, plus a stdio smoke test of `initialize` / `session/new`. What has not
  happened is a session driven by a real editor against a real provider.

## Known-incomplete, tracked

These are shipped-but-partial, and the issue says which half is missing rather
than the row implying it is done.

| Item | What is missing | Issue |
|---|---|---|
| Windows shell containment | filesystem scoping; needs AppContainer or a restricted primary token, either of which means owning `CreateProcessW` instead of spawning through `tokio::process` | [#124](https://github.com/vedantnimbarte/Wingman/issues/124) |
| ACP file writes | `fs/write_text_file`; the `/undo` checkpoint and audit-log writes have to move with it | [#127](https://github.com/vedantnimbarte/Wingman/issues/127) |
| Slack/email transports | live accounts to test against | [#32](https://github.com/vedantnimbarte/Wingman/issues/32) |
| Daemon `auto_dispatch` | a live provider run that opens a real PR from an issue; `pilot daemon --dry-run` covers the trust config safely in the meantime. The `pr_reviews` rework rounds in #247 ride the same unvalidated dispatch path | [#34](https://github.com/vedantnimbarte/Wingman/issues/34) |
| Pilot eval gate | `eval.yml` (in #252) compares against `eval/baseline.jsonl`, which can only come from a real paid eval run; until one is committed the workflow posts a notice and does not gate | — |
| J11 sandbox tiers | a real Docker daemon and a KVM host with a Firecracker kernel/rootfs; the tiers in #253 are argv/config/patch-back tested only | — |
