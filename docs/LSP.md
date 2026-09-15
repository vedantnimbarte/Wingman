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

`who_calls` itself uses the server when one is there: it asks for
`callHierarchy/incomingCalls` at the symbol's definition, then
`textDocument/references`, and only then name-matches. Its first output line
says which of the three answered.

The affected-tests gate and `wingman knows` use it too; see
[Verification receipts](#verification-receipts) and the staleness note below.

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
affected_tests  = true     # tests referencing the edited symbols, else the changed crates
lsp_diagnostics = true     # fold changed-file LSP diagnostics into the gate
```

After a turn that edited files, the gate runs compile → affected tests → LSP
diagnostics, short-circuiting on the first failure. The LSP stage collects
diagnostics for the files changed this turn and **fails on any error** (severity
1), so a change that introduces a type error the compile command didn't surface
— or a change in a language with no cheap compile step — is caught before the
agent is allowed to say "done". Fail-open: no server installed, no changed
files, or a server hiccup all pass with a note rather than trapping the agent.

The affected-tests stage narrows to the tests that reference the symbols edited
this turn. It finds each edited function or type with tree-sitter, asks the
language server for its `textDocument/references`, and keeps the sites in test
code: files under `tests/`, lines below a file's first `#[cfg(..test..)]`, or a
file with no such `cfg` that has `#[test]` functions (a test module in its own
file). It follows the helpers and fixtures around those sites through test code
by name, so a test calling a helper that calls the edit is found too, and runs
just the matching tests with `cargo test -- --exact`. With no server, or one
that errors or answers nothing for any symbol (a cold server still indexing),
it name-matches the edited symbols in test code instead. The receipt names
which one mapped the tests:

```text
edited symbols: parse
narrowed via LSP textDocument/references (test helpers by name) to 1 test(s) referencing them: tests::parses
$ cargo test --quiet -p foo -- --exact tests::parses
```

It runs the whole changed crates instead, and the receipt says why, when the
narrowed set could miss a test: a new, deleted or non-Rust file inside a crate,
a line outside any function or type (a `use`, a doc comment that may be a doc
test), a doc test in a changed crate that uses an edited symbol (in a doc
comment or a file included with `#[doc = include_str!(..)]`; doc tests can't be
named on the `--exact` line), no test referencing an edited symbol, or more than
64 matching tests. Production code is not followed: a test that reaches the
edit only through a non-test function is not mapped.

`wingman knows` flags a project memory as stale when it names a code symbol the
project no longer has, as well as a file that is gone (global memories get only
the file check). A symbol counts as present when a source file defines it or
still mentions it (a `PathBuf` imported from the standard library is not
stale); a name found neither way is put to each installed server's
`workspace/symbol` before it is reported.

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
