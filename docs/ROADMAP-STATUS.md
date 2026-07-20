# Roadmap Status

Tracks the differentiation roadmap: what shipped, and what remains with a
concrete plan. "Shipped" items are on `main` (or an open PR), tested, and
CI-green.

## Shipped

| Item | What | Where |
|---|---|---|
| MSRV honesty | Declared floor set to the real 1.88; gate re-enabled | `Cargo.toml`, `ci.yml` |
| LSP code-actions | `lsp_code_action` (quick-fixes / organize-imports); client applies `workspace/applyEdit` | `wingman-lsp`, `wingman-tools` |
| Wingman-as-MCP-server | `wingman mcp-serve` exposes tools + memory over MCP stdio | `commands/mcp_serve.rs` |
| Git-native auto-commit | `[git].auto_commit` — Aider-style per-turn commits | `git_auto.rs` |
| Local-first preset | `wingman router preset local` + `local` class keyword | `commands/router.rs` |
| Explain-and-teach | `wingman explain` — per-file what/why of the diff | `commands/explain.rs` |
| Benchmark harness | `wingman bench` — TTFT / tokens / verified-done | `commands/bench.rs` |
| Affected-tests receipt | Edited symbols surfaced in the gate receipt (tree-sitter) | `runtime.rs` |
| Agent SDK docs | Embed `wingman-core`; drive over MCP | `docs/SDK.md` |
| Audit trail | `[audit].enabled` — JSONL compliance log of tool calls | `wingman-tools/registry.rs` |

## Remaining — with plan

These are genuinely larger and/or need infrastructure that can't be built and
validated inside a single coding session. Each has a concrete design so it can
be picked up directly.

### Duplicate `reqwest` (0.12 + 0.13)
Deliberately deferred — see `docs/DEPENDENCIES.md`. Unifying means migrating five
crates to `reqwest 0.13` (feature rename `rustls-tls`→`rustls` + API changes)
purely to satisfy a transitive `rmcp` bump. Do it as a focused PR with a full
re-verify, not as a side effect. `deny.toml` warns until then.

### Safe symbol-level affected-tests narrowing
The receipt now lists edited symbols, but the `cargo test` run stays
crate-level. Narrowing to symbol-name filters risks a **false green** (a filter
matching no test runs zero tests and passes). Safe narrowing needs resolved
`test-fn → edited-symbol` references (via `lsp_references` on each edited symbol,
mapping referrers under `#[cfg(test)]`/`tests/` to their test targets), then
running exactly those. Plan: reuse the LSP client's `references`, filter to test
items, map file→`-p crate` + `--test <name>`, and fall back to crate-level when
the map is empty.

### Browser/visual verification (Cursor-style)
A verification-gate stage that drives a headless browser to prove a UI change
renders. Plan: add a `wingman-browser` crate wrapping a CDP driver
(`chromiumoxide`), a `[verify].browser` config (URL + optional screenshot
baseline), and a `BrowserGate` that loads the URL, asserts no console errors,
and diffs a screenshot against a baseline. Gated behind a feature flag since it
needs a Chromium binary; CI would run it only on a runner with Chrome installed.

### Editor bridge (VS Code / JetBrains)
A thin extension talking to `wingman mcp-serve` (or `wingman-core` via a local
socket). Plan: a separate `editors/vscode` TypeScript project using the MCP
client to expose `semantic_search`, `lsp_*`, memory, and a chat panel; ship via
the marketplace. This lives in its own toolchain/release pipeline, so it's a
separate deliverable from the Rust workspace.

### Server-backed team memory + policy (enterprise)
Audit logging shipped. The remaining piece is optional server-backed memory
sync (beyond the git-backed `wingman memory sync`) with auth and per-repo
policy. Plan: a small sync service + `[team]` config (endpoint + token); the
client pushes/pulls memory packs and honors a server-provided policy document
(allowed tools/models per repo). Needs a hosted service to validate end-to-end.

### Pilot daemon: external intake/notify + voice (issues #31–35)
The daemon's file-drop intake + webhook notify exist; the open items are live
Slack/email transports, live-validated `auto_dispatch`, and mic-capture voice
intake. These need external service credentials and audio hardware, so they're
validated outside CI. Plan: implement the Slack Events API + SMTP/IMAP adapters
behind `[pilot.daemon.transports]`, and a `wingman pilot listen --voice` front
end using a local STT model.
