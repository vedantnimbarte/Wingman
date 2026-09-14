# Tree-Sitter Integration Guide

Wingman integrates tree-sitter (`wingman-ts` crate) for language-aware code understanding across multiple subsystems.

## Overview

The `wingman-ts` crate provides a minimal facade over tree-sitter and language grammars, hiding transitive dependencies and allowing feature-gated opt-out for builds that don't need parsing.

**Supported languages:**
- Rust, Python, JavaScript, TypeScript, Go, C++, Java, Kotlin

**Core abstractions:**
- `Language` enum — file path → detected language.
- `Symbol` struct — name, kind, line range, signature.
- Parsing functions — extract symbols, semantic chunks, outline, find enclosing scope.

## Architecture

### Design Principles

1. **Minimal Public API** — Consumers see only `Language`, `Symbol`, and a handful of free functions. Tree-sitter crate is not re-exported.
2. **Feature-Gated** — Behind the `treesitter` feature (enabled by default). Builds without the C toolchain if disabled.
3. **Graceful Degradation** — All functions have fallback implementations that return empty Vec/None, so call sites stay parser-agnostic.
4. **Reusable Across Crates** — Used by RAG (semantic chunking), tools (diffs), TUI (outline), and learning hooks.

### Key Types

**`Language` enum** (`crates/wingman-ts/src/lang.rs`):
```rust
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Cpp,
    Java,
    Kotlin,
}

impl Language {
    pub fn from_path(path: &Path) -> Option<Self>;  // None for unknown extensions
    pub fn from_extension(ext: &str) -> Option<Self>;
}
```

**What each language extracts:**

| Language | Extensions | Symbols |
|----------|------------|---------|
| Rust | `rs` | fn, struct, enum, trait, impl, mod, const/static, type |
| Python | `py`, `pyi` | def, class |
| JavaScript / TypeScript / TSX | `js`, `jsx`, `mjs`, `cjs`, `ts`, `mts`, `cts`, `tsx` | function, method, class, interface, type alias |
| Go | `go` | func, method, type |
| C++ | `cc`, `cpp`, `cxx`, `hpp`, `hh`, `hxx` | function definitions and prototypes, methods (in-class and `Foo::bar` out-of-class), class, struct/union, enum, namespace, `using`/`typedef` aliases |
| Java | `java` | class, record, interface, `@interface`, enum, method, constructor |
| Kotlin | `kt`, `kts` | fun (extension functions by their own name), class, interface, enum class, object, typealias |

`.h` is deliberately unmapped: it is as often C as C++, and C is not a parsed
language. Functions nested in a class, struct, enum, impl, or namespace are reported as
methods; a forward declaration (`class Foo;`) is not a symbol.

**`SymbolKind` enum** (`crates/wingman-ts/src/symbol.rs`):
```rust
pub enum SymbolKind {
    Function, Method, Struct, Enum, Trait, Impl,
    Class, Interface, TypeAlias, Module, Constant, Variable,
}

impl SymbolKind {
    pub fn label(self) -> &'static str; // e.g., "fn", "struct"
}
```

**`Symbol` struct** (`crates/wingman-ts/src/symbol.rs`):
```rust
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: u32,       // 1-based, inclusive
    pub end_line: u32,
    pub start_byte: usize,     // half-open [start, end)
    pub end_byte: usize,
    pub signature: String,     // first line of declaration
}
```

**`SemanticChunk` struct** (tree-sitter feature only):
```rust
pub struct SemanticChunk {
    pub start_line: u32,
    pub end_line: u32,
    pub start_byte: usize,
    pub end_byte: usize,
    pub symbol: Option<Symbol>, // the function/class containing this chunk
    pub content: String,        // full source text of the chunk
}
```

### Core Functions

When `treesitter` feature is enabled:

| Function              | Purpose                                                  |
|-----------------------|----------------------------------------------------------|
| `extract_symbols`     | Parse a file → Vec<Symbol> of all top-level definitions. |
| `semantic_chunks`     | Parse a file → Vec<SemanticChunk> (function bodies, etc). |
| `outline`             | Generate a markdown outline (one symbol per line).        |
| `enclosing_symbol`    | Find the function/class containing a given line number.   |
| `replace_function_body` | Refactor a named function's body in-place.             |
| `ParserPool`          | Reusable thread-local parser cache.                       |
| `TreeCache`           | Per-file parse trees; `semantic_chunks` reparses incrementally. |
| `highlight::highlight` | Scope spans for a source string (`highlight` feature).  |

When feature disabled, all return empty Vec/None (inert fallbacks).

## Integration Points

### 1. RAG Indexing (`wingman-rag`)

**Purpose:** Tree-sitter powers semantic chunking so the index groups related code together.

**Usage flow:**
```
Project scan → find source file
    ↓
detect Language via file extension/shebang
    ↓
If [features] treesitter enabled:
    ├─ parse with tree-sitter
    ├─ extract semantic chunks (function bodies, class members)
    └─ embed each chunk (with context: enclosing symbol)
Else:
    └─ fallback: slide window chunking by lines
    ↓
Insert into SQLite with embedding vector
```

**Relevant code:**
- `crates/wingman-rag/src/chunker.rs` — `Chunker` holds a `TreeCache`, so the
  watcher's re-chunk of a file that changed (an `edit_file` write, a save in an
  editor) reparses incrementally from the tree it kept for that file.
- RAG index queries return chunks with symbol context (e.g., "in function foo()").

### 2. Tool Layer (`wingman-tools`)

**Purpose:** Tree-sitter supports AST-aware diffs and symbol replacement.

**Tools that use tree-sitter:**
- `edit_file` — when replacing a function body, parse to ensure valid scope.
- Diff review tools — AST-aware hunks (coming in M8).

**Relevant code:**
- `crates/wingman-tools/src/builtin/edit_file.rs` — calls `replace_function_body()`.

### 3. Diff/Review (`wingman-cli` commands)

**Purpose:** `wingman diff` and `wingman review` use tree-sitter for syntax-aware hunk review.

**Features:**
- Display only changed function signatures (not full diff).
- Outline of changed symbols.
- Language-aware hunk boundaries.

**Relevant code:**
- `crates/wingman-cli/src/commands/diff.rs` — interactive hunk review.
- `crates/wingman-cli/src/commands/diff_annotate.rs` — tree-sitter outline generation.

### 4. TUI Syntax Highlighting (`wingman-tui`)

**Purpose:** Code in the terminal UI is coloured by syntax scope.

**Features:**
- Fenced code blocks in the transcript, by the fence's info string
  (`rust`, `rs`, `c++`, `kotlin`, or any extension `Language` knows).
- The file view: `v` on a file in the `Ctrl+B` sidebar opens it read-only,
  line-numbered and highlighted by extension (the first 512 KiB).
- Scopes map onto the theme: `default` and `light` have their own palettes,
  `mono` and `NO_COLOR` tell scopes apart by bold/italic/dim only.
- A `diff` fence is not highlighted: added and removed lines in green and red
  would read as passed and failed (see decisions/0016).

**Relevant code:**
- `crates/wingman-ts/src/highlight.rs` — runs `tree-sitter-highlight` with each grammar's bundled query.
- `crates/wingman-tui/src/widgets/code.rs` — maps scopes to styles from the theme.
- `crates/wingman-tui/src/modal/file_view.rs` — the file view.

### 5. Learning Loop (`wingman-learn`)

**Purpose:** Extract skill patterns from session logs using tree-sitter context.

**Features:**
- When a tool call sequence is repeated (e.g., grep → read → edit), extract context (function/class being edited).
- Skills include symbol-scoped instructions (e.g., "when refactoring a function, call this sequence").

**Relevant code:**
- `crates/wingman-learn/src/extract.rs` — mine repeated patterns from sessions.

## Usage Examples

### Example 1: Extract Symbols from a Rust File

```rust
use wingman_ts::{Language, extract_symbols};

let src = r#"
fn main() {
    println!("hello");
}

struct Point { x: i32, y: i32 }

impl Point {
    fn new(x: i32, y: i32) -> Self { Self { x, y } }
}
"#;

let symbols = extract_symbols(Language::Rust, src);
// symbols = [
//   Symbol { name: "main", kind: Function, start_line: 2, ... },
//   Symbol { name: "Point", kind: Struct, start_line: 6, ... },
//   Symbol { name: "new", kind: Method, start_line: 9, ... },
// ]
```

### Example 2: Generate an Outline

```rust
use wingman_ts::{Language, outline};

let src = "fn foo() { } struct Bar { }";
let md = outline(Language::Rust, src);
// md = Some("- Function: foo (1:1-1:14)\n- Struct: Bar (1:21-1:33)\n")
```

### Example 3: Semantic Chunking

```rust
use wingman_ts::{Language, semantic_chunks};

let src = "fn foo() { a(); } fn bar() { b(); }";
let chunks = semantic_chunks(Language::Rust, src);
// chunks = [
//   SemanticChunk { symbol: Some(Symbol { name: "foo", ... }), content: "fn foo() { a(); }", ... },
//   SemanticChunk { symbol: Some(Symbol { name: "bar", ... }), content: "fn bar() { b(); }", ... },
// ]
```

### Example 4: Building Without Tree-Sitter

If you're building a tool that doesn't need parsing, disable the feature:

```bash
# In a dependent crate's Cargo.toml:
wingman-ts = { workspace = true, default-features = false }
```

All functions will return empty Vec/None. Your code stays branch-free:

```rust
let symbols = extract_symbols(Language::Rust, src);
// symbols is always Vec::new() when feature disabled.
for symbol in symbols {
    // This loop is optimized away at compile time.
}
```

## Performance

### Parser Pool

`ParserPool` is a thread-local cache of tree-sitter parsers (one per language). Reusing parsers is faster than creating new ones:

```rust
let pool = ParserPool::new();
let symbols1 = pool.extract_symbols(Language::Rust, src1)?;
let symbols2 = pool.extract_symbols(Language::Rust, src2)?;
// Parser for Rust is reused; second call is faster.
```

### Incremental Reparse

`TreeCache` keeps the last tree for each file (up to 4 MiB of source across
all files, least recently parsed dropped first). When a file comes back
changed, the text between the unchanged prefix and suffix is described to the
old tree as one `tree_sitter::InputEdit`, and the parser reuses every subtree
outside it. Tests compare each incremental tree with a from-scratch parse of
the same text (`to_sexp`, and the resulting chunks) across inserts, deletes,
broken-then-repaired syntax and multi-byte text. To see the time saved on a
3000-function file:

```bash
cargo test -p wingman-ts incremental_reparse_timing -- --nocapture
```

### Embedding Cost

Tree-sitter parsing adds ~5-10ms per file (typical sizes <10KB). For projects with thousands of files, semantic chunking is deferred to a background task (e.g., at agent startup).

### Fallbacks

If tree-sitter parsing fails (e.g., syntax error in the source), the system falls back to line-window chunking. This is transparent to the caller.

## Testing

Tree-sitter parsing is tested in `crates/wingman-ts/tests/`:

```bash
# Run tree-sitter tests (requires feature enabled)
cargo test --features treesitter -p wingman-ts
```

Test cases cover:
- Basic symbol extraction for each language.
- Semantic chunking correctness.
- Outline generation.
- Edge cases (empty files, syntax errors, nested scopes).

## Future Enhancements

- **Language expansion** — add C, C#, Ruby, etc.
- **Custom queries** — user-defined tree-sitter queries for domain-specific extraction.

## Troubleshooting

### Q: Parsing fails for some files. Why?

**A:** Tree-sitter expects syntactically valid source. Incomplete code (e.g., user typing interactively) may fail to parse. The system falls back gracefully to line-window chunking, so RAG and tools continue to work.

### Q: How do I disable tree-sitter to speed up builds?

**A:** In `Cargo.toml`:
```toml
wingman-ts = { workspace = true, default-features = false }
```

Or at the workspace level:
```bash
cargo build --no-default-features
```

This removes the C toolchain dependency. All tree-sitter functions become no-ops.

### Q: Can I add support for language X?

**A:** Yes. In `crates/wingman-ts/src/lang.rs`:
1. Add variant to `Language` enum and its extensions to `from_extension()`.
2. Add a grammar crate built against the workspace's `tree-sitter` version to `Cargo.toml` (behind `treesitter` feature) and note it in `docs/DEPENDENCIES.md`.
3. In `crates/wingman-ts/src/parse.rs`: the grammar in `ts_language()`, a `<lang>_symbol()` mapping node kinds to `SymbolKind`, container kinds in `descends_into()`, and function kinds in `is_function_like()`.
4. Add its highlight query to `config_for()` in `highlight.rs`.

The rest of the codebase is language-agnostic.
