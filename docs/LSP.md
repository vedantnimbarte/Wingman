# LSP-backed Code Intelligence

Wingman drives real Language Server Protocol servers for **resolved** code
intelligence — go-to-definition, find-references, hover, diagnostics, and
project-wide rename — the semantic upgrade over the tree-sitter heuristics
(`find_symbol`, `who_calls`). Where tree-sitter *name-matches*, a language
server *resolves*: it follows imports, re-exports, and inferred types, and it
won't confuse two different symbols that share a name.

## How it works

Wingman does **not** bundle language servers. It launches whatever server you
already have on `PATH`, so a team standardizes on the same servers their editors
use and the binary stays small.

| Language              | Server (first found on PATH)                              |
|-----------------------|-----------------------------------------------------------|
| Rust                  | `rust-analyzer`                                            |
| Python                | `pyright-langserver` → `pylsp` → `jedi-language-server`    |
| JavaScript / TypeScript | `typescript-language-server`                            |
| Go                    | `gopls`                                                   |

When no server is installed for a file's language, the LSP tools return a short
note telling the agent to fall back to `find_symbol` / `who_calls` — a graceful
degrade, not an error.

The client (`wingman-lsp`) speaks JSON-RPC over stdio directly (raw wire JSON,
no protocol-types dependency), performs the `initialize`/`initialized`
handshake, opens documents on demand, and keeps one warm server per language per
project root (pooled process-wide, so repeated tool calls reuse it).

## Tools (callable by the agent)

| Tool              | What it does                                                        |
|-------------------|--------------------------------------------------------------------|
| `lsp_definition`  | Resolve where a symbol is **defined** (follows imports/types).     |
| `lsp_references`  | Every **resolved** reference across the project (not name matches).|
| `lsp_hover`       | Type / signature / doc summary the server shows on hover.          |
| `lsp_diagnostics` | Live errors/warnings for a file — the editor's red squiggles.      |
| `lsp_rename`      | Rename a symbol project-wide, updating every reference atomically. Needs write permission. |

**Ergonomics.** Position-taking tools accept `path` + `line` plus **either** a
1-based `character` column **or** a `symbol` name to locate on that line. The
`symbol` form is what models produce reliably (exact UTF-16 columns are
error-prone), e.g.:

```json
{ "path": "src/agent.rs", "line": 42, "symbol": "AgentLoop" }
```

`lsp_rename` is gated on the write permission (`auto-edit`/`yolo`); the read
tools are gated on read permission like any other file read.

## Verification receipts

Set under `[verify]` in config:

```toml
[verify]
turn_gate       = "auto"   # compile check (cargo check / tsc --noEmit / …)
affected_tests  = true     # tests of the changed crates
lsp_diagnostics = true     # fold changed-file LSP diagnostics into the gate
```

After a turn that edited files, the gate runs compile → affected tests → LSP
diagnostics, short-circuiting on the first failure. The LSP stage collects
diagnostics for the files changed this turn and **fails on any error** (severity
1), so a change that introduces a type error the compile command didn't surface
— or a change in a language with no cheap compile step — is caught before the
agent is allowed to say "done". Fail-open: no server installed, no changed
files, or a server hiccup all pass with a note rather than trapping the agent.

## Notes & limits

- Diagnostics are published asynchronously after a document opens; a cold server
  (e.g. rust-analyzer indexing) may take a few seconds on first use. The gate
  and `lsp_diagnostics` allow a generous timeout and pass with a note on
  timeout rather than blocking.
- `lsp_rename` applies the server's `WorkspaceEdit` directly to disk (UTF-16
  offset aware); pair it with `wingman rewind` / checkpoints if you want an easy
  undo.
- The set of languages mirrors what `wingman-ts` parses, so LSP is a strict
  upgrade path over the heuristic symbol tools.
