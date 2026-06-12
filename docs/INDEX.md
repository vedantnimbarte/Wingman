# Arc-Code Documentation Index

Welcome to Arc-Code's technical documentation. This index guides you to the right resource based on what you're trying to do.

## Getting Started

- **New to Arc-Code?** Start with the [README.md](../README.md) for an overview, quick start, and highlights.
- **Installation issues?** See the [Installation](#installation) section in README.
- **First run?** See the [Quick Start](#quick-start) section in README, then try `arccode --help`.

## Understanding Arc-Code

### Architecture & Design

- **[ARCHITECTURE.md](ARCHITECTURE.md)** — System overview, core crates, data flow diagrams, threading model, feature flags.
  - Best for: Understanding how components fit together, design decisions, module responsibilities.
  - Read this if: You're extending Arc-Code, debugging, or understanding the agent loop.

### Major Subsystems

- **[TREE-SITTER.md](TREE-SITTER.md)** — Language-aware parsing integration (Rust, Python, JavaScript, TypeScript, Go).
  - Best for: Understanding semantic chunking, symbol extraction, code understanding.
  - Read this if: You're working with the RAG index, diff tools, or adding new languages.

- **[LEARNING-LOOP.md](LEARNING-LOOP.md)** — Self-improving mechanism (memories, skill stats, session recall, nudges).
  - Best for: Understanding how Arc-Code learns from sessions, persists across projects, and builds institutional knowledge.
  - Read this if: You're using memory features, skill extraction, or cross-project recall.

- **[TOOLS.md](TOOLS.md)** — Reference for all 20+ built-in tools (file I/O, search, shell, semantic search, memory, skills, session management).
  - Best for: Complete tool signatures, behavior, permission requirements, error handling.
  - Read this if: You want to know what tools are available or how to use a specific tool.

### Planned Features

- **[AUTONOMOUS-MODE.md](AUTONOMOUS-MODE.md)** — Planned multi-task agent orchestration (M8).
  - Best for: Understanding the vision, data model, architecture, TUI integration.
  - Read this if: You're interested in the roadmap or contributing to autonomous mode.

- **[DIFFERENTIATION.md](DIFFERENTIATION.md)** — Differentiation roadmap (model routing, warm repo index, verification receipts, team memory).
  - Best for: Understanding how Arc-Code plans to beat Claude Code/Codex on speed, trust, and retention.
  - Read this if: You're prioritizing single-agent features or positioning the product.

## Using Arc-Code

### Command Reference

See **CLI Reference** in [README.md](../README.md#cli-reference) for all subcommands.

Key commands:
- `arccode` — launch TUI.
- `arccode --print "<prompt>"` — one-shot headless mode.
- `arccode config init` — scaffold config.
- `arccode review <pr#>` — code review.
- `arccode session list` — browse past sessions.
- `arccode memory list` — view memories.
- `arccode knows` — show what Arc-Code knows about this project (memories, skills, routing, turn gate, index freshness).
- `arccode skill extract` — mine skills from sessions.
- `arccode discover` — find local LLMs.

### Configuration

See **Configuration** in [README.md](../README.md#configuration) for:
- Layered config resolution (defaults → global → project → env vars → CLI flags).
- Config sections (tokens, router, tui, providers, hooks, schedule, autonomous).
- Permission modes (read-only, plan, auto-edit, yolo).
- Hooks (pre_tool_use, post_tool_use, stop, user_prompt_submit).

### TUI Usage

See **Inside the TUI** in [README.md](../README.md#inside-the-tui) for:
- Typing prompts and hitting Enter.
- Slash commands (`/model`, `/memory`, `/recall`, `/skill`, `/learn`).
- File sidebar (`Ctrl+B`).
- Themes and colors.
- Transcript search (`/find`, `/findnext`).

### Memory & Learning

See **Self-improving loop** in [README.md](../README.md#self-improving-loop) for:
- Memory types and scope.
- Skill usage tracking and outcome scoring.
- Session embeddings and cross-project recall.
- Nudges (quiet-session reminders).

Then read **[LEARNING-LOOP.md](LEARNING-LOOP.md)** for deep dives into each subsystem.

## Contributing to Arc-Code

### Development Setup

```bash
git clone https://github.com/vedantnimbarte/Arc-Code.git
cd Arc-Code
cargo build --release
cargo test
```

See **Development** in [README.md](../README.md#development) for build, test, and run commands.

### Code Organization

Read **[ARCHITECTURE.md](ARCHITECTURE.md)** → **Crate Responsibilities** for:
- What each crate does.
- Where to find relevant code.
- Module boundaries and public APIs.

### Key Crates at a Glance

| Crate                | Purpose                                                |
|----------------------|--------------------------------------------------------|
| `arccode-cli`        | Binary entry point, CLI args, command dispatch.        |
| `arccode-core`       | Agent loop, Provider trait, tool dispatch.             |
| `arccode-config`     | Layered config loading, permission model.              |
| `arccode-providers`  | Nine LLM provider adapters.                            |
| `arccode-tools`      | 20+ built-in tools + registry.                         |
| `arccode-tui`        | Interactive ratatui surface.                           |
| `arccode-session`    | Append-only JSONL session logging.                     |
| `arccode-rag`        | Semantic code index (SQLite + embeddings).             |
| `arccode-skills`     | Markdown skill library (global + project).             |
| `arccode-learn`      | Memory store, skill stats, session embedding.          |
| `arccode-ts`         | Tree-sitter facade (language parsing).                 |
| `arccode-mcp`        | MCP host scaffolding (early stage).                    |

### Adding a New Tool

1. Create a new file in `crates/arccode-tools/src/builtin/<tool_name>.rs`.
2. Implement the `Tool` trait (`spec()` and `run()`).
3. Register in `crates/arccode-tools/src/registry.rs`.
4. Update **[TOOLS.md](TOOLS.md)** with the tool signature and examples.

### Adding Support for a Language

1. Update `crates/arccode-ts/Cargo.toml` — add grammar crate.
2. Update `crates/arccode-ts/src/lang.rs` — add Language variant, detection.
3. Update `crates/arccode-ts/src/parse.rs` — add parser initialization.
4. Update **[TREE-SITTER.md](TREE-SITTER.md)** with the new language.

## Feature Flags

Arc-Code uses feature flags to reduce build time and dependencies for builds that don't need certain subsystems.

```bash
# Build with all features (default)
cargo build --release

# Build without tree-sitter (faster, but no language parsing)
cargo build --release --no-default-features -p arccode-ts

# Build without embeddings (uses hash fallback for RAG)
cargo build --release --no-features embeddings -p arccode-rag
```

See **[ARCHITECTURE.md](ARCHITECTURE.md)** → **Feature Flags** for details.

## Troubleshooting

### Build Issues

**Windows MSVC: `error LNK1318` (PDB cap exceeded)**

See `Cargo.toml` — already configured with `debug = "line-tables-only"` to stay under the 4GB limit.

**Tree-sitter compilation fails**

Tree-sitter requires a C toolchain. Install Visual Studio Build Tools (Windows) or gcc (Unix), or disable tree-sitter:
```bash
cargo build --release --no-default-features
```

### Runtime Issues

**"No such file or directory" errors**

Arc-Code expects absolute paths. Use `glob_tool` or `list_dir` to find files, then pass absolute paths to other tools.

**Permission errors**

Check your permission mode (`arccode config show | grep permission_mode`). Adjust in config.toml or pass `--mode auto-edit`.

**Token limit exceeded**

Compaction is triggered when history exceeds `tokens.compact_at_tokens` (default 120k). Older turns are summarized. See **[ARCHITECTURE.md](ARCHITECTURE.md)** → **Token Management**.

**Session embedding is slow**

Session embedding happens asynchronously after the session ends. This is normal on first run. Set `[learn] embed_sessions = false` in config to disable.

## Performance Tuning

- **Large codebases:** Increase `tokens.compact_at_tokens` and `tokens.tool_output_max_lines` so tool output isn't truncated too early.
- **Fast inference:** Use `router.fast_model = "anthropic/claude-haiku-4-5-20251001"` for quick tasks (Haiku is cheap and fast).
- **RAG indexing:** Set `rag.chunk_size = 500` (default) for larger chunks, or `200` for more granular search.
- **Disable RAG:** Set `rag.enabled = false` if you don't use `semantic_search`.

## References

### External Resources

- **Rust docs:** https://doc.rust-lang.org/
- **Tokio async:** https://tokio.rs/
- **Ratatui TUI:** https://ratatui.rs/
- **Tree-sitter:** https://tree-sitter.github.io/tree-sitter/
- **SQLite:** https://www.sqlite.org/
- **Anthropic Messages API:** https://docs.anthropic.com/messages/

### Internal References

- **Plan (M8+ roadmap):** `plan.md` in repository root.
- **Changelog (if any):** `CHANGELOG.md` (when created).
- **Contributing guidelines:** `CONTRIBUTING.md` (when created).

## Document Map

```
docs/
├── INDEX.md                 (you are here)
├── ARCHITECTURE.md          (system design + data flows)
├── TREE-SITTER.md           (language parsing integration)
├── LEARNING-LOOP.md         (memories, skills, session recall)
├── TOOLS.md                 (complete tool reference)
├── AUTONOMOUS-MODE.md       (planned multi-task orchestration)
└── DIFFERENTIATION.md       (single-agent differentiation roadmap)

../README.md                 (main project overview)
../plan.md                   (M8 implementation plan)
../Cargo.toml                (workspace manifest)
```

## Quick Search

Looking for:

- **How to save a memory?** → [LEARNING-LOOP.md](LEARNING-LOOP.md#saving-memories) or [TOOLS.md](TOOLS.md#save_memory).
- **How to use semantic search?** → [TOOLS.md](TOOLS.md#semantic_search) or [ARCHITECTURE.md](ARCHITECTURE.md#arccode-rag).
- **How permissions work?** → [README.md](../README.md#permission-modes) or [ARCHITECTURE.md](ARCHITECTURE.md#arccode-config).
- **How to add a new provider?** → See `crates/arccode-providers/src/` and read [ARCHITECTURE.md](ARCHITECTURE.md#arccode-providers).
- **What's tree-sitter doing?** → [TREE-SITTER.md](TREE-SITTER.md) or [ARCHITECTURE.md](ARCHITECTURE.md#arccode-ts).
- **How does the agent loop work?** → [ARCHITECTURE.md](ARCHITECTURE.md#agent-loop).
- **What's planned next?** → [AUTONOMOUS-MODE.md](AUTONOMOUS-MODE.md) or `../plan.md`.

## Feedback & Questions

If this documentation is unclear or missing something:
1. Check the code (`crates/*/src/lib.rs` files have doc comments).
2. See `git log` for recent changes and commit messages.
3. Open an issue on GitHub with your question.

---

**Last updated:** 2026-05-28 | **Arc-Code v0.0.1**
