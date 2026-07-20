# Roadmap Status

Tracks the differentiation roadmap. Everything below is implemented on `main` (or
an open PR), compiles, is clippy-clean, and has unit tests for its logic. Items
whose *runtime* needs external infrastructure (a browser binary, a mail system,
Slack, a hosted server) are noted — the code is complete and tested; only the
live end-to-end run needs that infrastructure.

## Shipped

| Item | What | Runtime needs |
|---|---|---|
| MSRV honesty (L1) | Declared floor set to 1.88; gate re-enabled | — |
| LSP code-actions (T1.1) | `lsp_code_action`; client applies `workspace/applyEdit` | a language server |
| Wingman-as-MCP-server (T1.2) | `wingman mcp-serve` (tools + memory resources) | — |
| Git-native auto-commit (T1.3) | `[git].auto_commit` | a git repo |
| Local-first preset (T3.7) | `wingman router preset local` + `local` class | a local model |
| Explain-and-teach (T3.8) | `wingman explain` | a provider |
| Benchmark harness (L5) | `wingman bench` | a provider |
| Affected-tests receipt (L3) | Edited symbols surfaced in the gate receipt | — |
| Agent SDK (T2.5) | `docs/SDK.md`; embed core or drive over MCP | — |
| Audit trail (T3.9) | `[audit].enabled` JSONL compliance log | — |
| **reqwest 0.13 unify (L2)** | All first-party crates on reqwest 0.13 + ring; only `hf-hub` (embeddings) keeps a transitive 0.12 | — |
| **Browser verification (T2.4)** | `wingman-browser` crate + `BrowserGate` (`[verify].browser`); screenshot diff vs baseline | a Chrome binary + `--features browser` |
| **Server-backed team memory (T3.9)** | `[team].endpoint` + `wingman memory push` / `pull` (non-clobbering merge) | a team memory HTTP endpoint |
| **Pilot Slack/email/voice intake (L4)** | `wingman pilot intake slack\|email\|voice` → intake files | Slack app / mail delivery / an STT transcript |
| **Editor bridge (T2.6)** | `editors/vscode` — VS Code extension over `wingman mcp-serve` | npm build + VS Code |

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
  TLS/ingress in front), email ingests `.eml` files your mail system delivers,
  and voice ingests any STT transcript file. A live mic front end still needs
  audio hardware + a local STT model, which any STT tool can supply via `voice`.
- **Editor bridge** — complete TypeScript extension (thin MCP client). Ships via
  the VS Code Marketplace on its own npm toolchain, separate from the Rust
  release pipeline; `npm install && npm run build` in `editors/vscode`.
