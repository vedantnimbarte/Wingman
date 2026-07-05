# Wingman Architecture Overview

Wingman is a modular, multi-provider coding agent written in Rust. This document describes its high-level architecture, core abstractions, and how subsystems interact.

## System Overview

Wingman operates on two surfaces:
1. **Interactive TUI** (`ratatui`-based) for long-lived sessions with state persistence and live interaction.
2. **Headless mode** (`--print`) for one-shot prompts that emit text or newline-delimited JSON events.

Both surfaces feed the same **agent loop** at `crates/wingman-core/src/agent.rs`, which orchestrates:
- **Provider abstraction** — speak to nine LLM backends (Anthropic, OpenAI, Gemini, etc.) through a unified `Provider` trait.
- **Tool dispatch** — route model tool calls to built-in tools (file I/O, shell, search, memory, skills, etc.).
- **Token management** — track token usage, estimate context, compact history when needed.
- **Learning hooks** — persist memories, track skill usage, embed session transcripts for recall.

## Core Crates

### `wingman-cli`
**Purpose:** Binary entry point, argument parsing, logging setup, surface selection.

**Key files:**
- `src/main.rs` — entry point, error handling.
- `src/cli.rs` — clap argument structure.
- `src/commands/` — subcommand handlers (config, init, session, worktree, memory, review, etc.).
- `src/commands/headless.rs` — runs `--print` mode.

**Responsibilities:**
- Parse CLI args and environment.
- Load global/project-local config via `wingman-config`.
- Select TUI or headless surface.
- Wire up tracing/logging.
- Dispatch to command handlers.

### `wingman-core`
**Purpose:** Provider-agnostic types, agent loop, tool dispatch, token pipeline.

**Key types:**
- `Message` — conversation message (role: assistant/user, blocks: text/tool_use/tool_result/image).
- `ContentBlock` — discriminated union of text, tool calls, results, etc.
- `Provider` trait — abstract interface every backend implements (streaming, tool use, caching).
- `ToolDispatcher` trait — agent loop asks tools registry to run tool calls.
- `CompletionRequest` — unified request shape for all providers.
- `AgentLoop` — runs the main loop: send message, stream completions, dispatch tool calls, collect results.
- `LearningHook` — before/after-turn hook for persistence (memories, stats, session embedding).

**Agent loop flow:**
```
1. Build CompletionRequest from system prompt, history, pending tool results.
2. Call provider.stream(request).
3. Yield StreamEvent::ContentBlock, ::ToolCall, ::Stop to event listener.
4. On ToolCall: look up tool in registry, run it, collect result.
5. On Stop: if tool calls pending, pack results into ToolResult blocks and loop.
   If no more tool calls, exit with final message.
```

**Key abstractions:**
- `CacheBreakpoint` / `CacheKind` — Anthropic-style prompt caching for cost reduction.
- `Compactor` — when history token count exceeds `compact_at_tokens`, summarize old turns.
- `ToolOutputBudget` — per-tool output size limits (head/tail truncation).

### `wingman-config`
**Purpose:** Layered config resolution, permission model, hook system.

**Config resolution (ascending priority):**
1. Built-in defaults.
2. `~/.wingman/config.toml` (global).
3. `<project>/.wingman/config.toml` (project).
4. `WINGMAN_*` environment variables.
5. CLI flags.

**Key sections:**
- `[tokens]` — `compact_at_tokens`, `tool_output_max_lines`, `prompt_cache`.
- `[router]` — `default_provider`, `default_model`, `fast_model`, `fallback_models`.
- `[tui]` — `theme`, `show_token_usage`.
- `[providers.<name>]` — API key, base URL, model per provider.
- `[hooks]` — lifecycle hooks (pre_tool_use, post_tool_use, stop, user_prompt_submit).
- `[[schedule]]` — cron entries for recurring tasks.
- `[autonomous]` — limits, role overrides, branch prefix (planned M8).

**Permission modes:**
| Mode         | Reads | Writes (in-tree) | Shell | Out-of-tree |
|--------------|-------|------------------|-------|-------------|
| `read-only`  | allow | prompt           | prompt | prompt      |
| `plan`       | allow | deny             | deny   | deny        |
| `auto-edit`  | allow | auto-allow       | auto-allow* | prompt   |
| `yolo`       | allow | auto-allow       | auto-allow | auto-allow  |

(*except denylist)

### `wingman-providers`
**Purpose:** Concrete `Provider` implementations for nine backends.

**Provider implementations:**

| Provider         | Class                  | Notes                                             |
|------------------|------------------------|---------------------------------------------------|
| Anthropic        | `AnthropicProvider`    | Reference: native tool use, explicit caching.    |
| OpenAI           | `OpenAiCompatProvider` | Variant: `OpenAi`.                                |
| ChatGPT (OAuth)  | `ChatGptProvider`      | Browser OAuth via `/login`; OS keychain storage. |
| OpenRouter       | `OpenAiCompatProvider` | Variant: `OpenRouter`. Aggregator model.          |
| LiteLLM          | `OpenAiCompatProvider` | Variant: `LiteLLM`. Gateway.                      |
| LM Studio        | `OpenAiCompatProvider` | Variant: `LmStudio`. Local OpenAI shim.           |
| vLLM             | `OpenAiCompatProvider` | Variant: `Vllm`. Inference server.                |
| Ollama           | `OpenAiCompatProvider` | Variant: `Ollama`. Localhost:11434.               |
| Google Gemini    | `GeminiProvider`       | Native adapter.                                   |

**Design pattern:**
- Implement `Provider::stream()` → yields `StreamEvent` (ContentBlock, ToolCall, Stop, Error).
- All providers return the same `Message` shape, allowing seamless model swaps.
- OpenAI-compatible backends share code via `OpenAiCompatProvider::new(variant)`.

### `wingman-tools`
**Purpose:** Built-in tool implementations and registry.

**Tool registry (`ToolRegistry`):**
- Implements `ToolDispatcher` trait for the agent loop.
- Checks permission mode before running each tool.
- Runs hooks (`pre_tool_use`, `post_tool_use`).
- Truncates output per `tool_output_max_lines`.

**Built-in tools:**

| Tool              | Purpose                                  |
|-------------------|------------------------------------------|
| `read_file`       | Read file by path; returns with line #s. |
| `write_file`      | Create/overwrite file.                   |
| `edit_file`       | Exact string replacement in a file.      |
| `apply_patch`     | Multi-file atomic edit (Update/Add/Del). |
| `glob_tool`       | Find files by glob pattern.              |
| `grep_tool`       | Content search (ripgrep semantics).      |
| `list_dir`        | List directory.                          |
| `run_shell`       | Execute shell command.                   |
| `web_fetch`       | Download URL, strip HTML.                |
| `web_search`      | DuckDuckGo search (no key).               |
| `semantic_search` | RAG index cosine search.                 |
| `present_plan`    | Structured plan; required in plan mode.  |
| `spawn_subagent`  | Inner agent on a sub-task (depth=1).     |
| `save_memory`     | Persist memory across sessions.          |
| `recall_memory`   | Fetch memory body by slug.                |
| `forget_memory`   | Delete memory.                           |
| `invoke_skill`    | Load skill for current turn.              |
| `recall_session`  | Semantic search over past sessions.      |
| `read_session`    | Fetch full session JSONL by id.           |

### `wingman-tui`
**Purpose:** Interactive `ratatui` surface with composer, transcript, sidebar, themes.

**Key components:**
- **Composer** — input box at bottom; `/` prefix triggers slash commands.
- **Transcript** — scrollable conversation history (model and user messages, tool output).
- **File sidebar** — `Ctrl+B` toggles; file browser for quick path insertion.
- **Status bar** — token usage, model/provider, mode, theme.
- **Welcome screen** — initial prompt hint.
- **Themes** — default, light, mono; per-role color overrides.

**Event handling:**
- Keyboard input fed to the composer (or sidebar if active).
- `Enter` submits prompt, triggers agent loop in a background task.
- Agent events (ContentBlock, ToolCall, Stop) update transcript in real time.
- `Ctrl+C` or `Ctrl+D` exits.

### `wingman-session`
**Purpose:** Append-only JSONL session log for reproducibility and recall.

**Session format (one JSON object per line):**
```json
{"role":"user","content":"explain the agent loop"}
{"role":"assistant","content":"The agent loop..."}
{"role":"assistant","tool_calls":[{"id":"tool_abc","name":"read_file","input":{"path":"..."}}]}
{"role":"user","tool_results":[{"tool_use_id":"tool_abc","content":"..."}]}
```

**Features:**
- `wingman session list` — browse recent session files.
- `wingman session fork [--at N]` — copy and optionally truncate.
- Sessions are embedded and indexed for `/recall` and cross-project search.

### `wingman-rag`
**Purpose:** Semantic code index via embeddings (SQLite + fastembed or hash fallback).

**Storage:**
- `<project>/.wingman/index.db` (SQLite with vec support).
- Schema: documents (file:// URIs, line ranges), embeddings (1536-dim or hash).

**Chunking:**
- Tree-sitter powered semantic chunking (functions, classes, modules).
- Fallback: simple line-window chunking if tree-sitter unavailable.
- Embedder options: `fastembed` (BGE small, ~90MB downloaded once) or deterministic hash.

**Usage:**
- `semantic_search` tool (callable by agent) → top-K cosine/hash-distance hits.
- Session transcript embedding for `recall_session` cross-project search.

### `wingman-skills`
**Purpose:** Markdown skill library (global + project-scoped).

**Skill file format:**
```markdown
---
name: my-skill
description: Does X
type: prompt
---

When the user asks for Y, respond with Z and call these tools:
1. read_file(...)
2. edit_file(...)
```

**How it works:**
- Skills auto-load from `~/.wingman/skills/*.md` and `<project>/.wingman/skills/*.md`.
- Names in the catalog are injected into the system prompt at every turn.
- Agent can call `invoke_skill` to fetch and use a skill body.
- Project-scoped skills override globals by name.

### `wingman-learn`
**Purpose:** Self-improving loop — persistent memories, skill stats, session recall, hooks.

**Four modules:**

| Module            | Role                                         |
|-------------------|----------------------------------------------|
| `memory`          | Markdown-frontmatter memory store (global/project). |
| `stats`           | SQLite skill usage + outcome tracking.      |
| `session_index`   | Embed and store finished sessions for recall. |
| `hooks`           | LearnHook impl; wires into agent loop.       |

**Memory types:**
| Type        | Scope   | Purpose                                |
|-------------|---------|----------------------------------------|
| `user`      | global  | Facts about the human.                 |
| `feedback`  | global  | How to behave (prefs, constraints).    |
| `project`   | project | Facts about this codebase.             |
| `reference` | global  | External pointers (issue tracker, etc).|

**Memory files (example):**
```
~/.wingman/memory/
├── MEMORY.md
│   ├── [user-role](user_role.md) — Senior Rust engineer
│   ├── [feedback-testing](feedback_testing.md) — Avoid mocks; use real DB
│   └── …
├── user_role.md
├── feedback_testing.md
└── …

<project>/.wingman/memory/
├── MEMORY.md
├── project_build_command.md
└── …
```

**Skill stats (`~/.wingman/learn.db`):**
- Every `invoke_skill` recorded with outcome (success/corrected/unclear).
- Outcomes derived from heuristics ("no,", "wait,", "wrong," in next turn).
- Skills crossing 3 invocations + 50% correction rate flagged for rewrite.

**Session embedding:**
- Finished sessions chunked and embedded into `~/.wingman/sessions.db`.
- `recall_session` tool searches this index across projects.

### `wingman-ts`
**Purpose:** Tree-sitter facade for language-aware parsing.

**Supported languages:**
- Rust, Python, JavaScript, TypeScript, Go.

**Key functions:**

| Function            | Purpose                                  |
|---------------------|------------------------------------------|
| `extract_symbols`   | Parse file → list of functions/classes/etc. |
| `semantic_chunks`   | Parse file → list of semantic chunks (function bodies, etc). |
| `outline`           | Generate markdown outline (one symbol per line). |
| `enclosing_symbol`  | Find function/class at a given line.     |
| `replace_function_body` | Refactor a function's body.           |

**Design:**
- Hidden behind `#[cfg(feature = "treesitter")]` so workspace builds without the C toolchain if not needed.
- Fallback functions return empty Vec/None when feature disabled.
- Used by `wingman-rag` for semantic chunking, `wingman-diff` for AST-aware diffs.

### `wingman-mcp`
**Purpose:** MCP host scaffolding (planned for M8+).

**Current state:** Early-stage framework for MCP tool integration. Not yet functional.

## Data Flow Diagrams

### Agent Loop

```
User Input (TUI or --print)
    ↓
[wingman-cli] parse args, load config
    ↓
[wingman-core AgentLoop] start
    ↓
Build CompletionRequest
    ├─ system prompt (+ skills index, memories index, nudges)
    ├─ conversation history (estimated tokens)
    └─ tool list
    ↓
[Provider] stream(request)
    ├─ Anthropic::stream → native tool_use + cache_control
    ├─ OpenAiCompatProvider::stream → function_call shape
    ├─ GeminiProvider::stream → functionCall shape
    └─ … (other 6)
    ↓
For each StreamEvent:
    ├─ ContentBlock → append to message
    ├─ ToolCall → lookup in registry, run tool
    │   ├─ [ToolCtx] check permission mode
    │   ├─ [Hook] pre_tool_use
    │   ├─ [Tool] run()
    │   ├─ [Hook] post_tool_use
    │   └─ Truncate output per budget
    ├─ ToolResult ← pack into ToolResult block
    └─ Stop → check if more tools pending
    ↓
On Stop (no more tool calls):
    ├─ [LearnHook] before_turn_complete
    │   ├─ Emit tool outcomes (success/error)
    │   ├─ Check for memory save requests
    │   └─ Embed turn into sessions.db
    ├─ Return final Message
    └─ Exit
    ↓
[TUI or headless] render output
```

### Configuration Resolution

```
Built-in defaults (in code)
    ↓
Load global ~/.wingman/config.toml
    ↓
Load project ./.wingman/config.toml (TOML sub-tables merge)
    ↓
Resolve ${ENV_VAR} placeholders
    ↓
Apply WINGMAN_* environment variables (override)
    ↓
Apply CLI flags (highest priority)
    ↓
Final Config
```

### Memory Lifecycle

```
User: "Remember that I use pnpm"
    ↓
Agent calls save_memory("user-pkg-manager", type=feedback, body="...")
    ↓
[wingman-learn] MemoryStore::save()
    ├─ Write to <scope>/memory/<slug>.md
    └─ Update <scope>/memory/MEMORY.md index
    ↓
Next session:
    ├─ Load MEMORY.md indices (global + project)
    ├─ Render indices into system prompt
    ├─ User/agent can call recall_memory(slug) → full body
    └─ Persist actions recorded in ~/ learn.db
```

## Threading Model

Wingman uses Tokio for async execution:

- **TUI:** spawns agent loop in a Tokio task, updates on event streams.
- **Headless:** single-threaded Tokio runtime, streams events to stdout/JSON.
- **Config/Session/Memory:** blocking I/O wrapped in `tokio::task::block_in_place`.
- **RAG embedding:** async `tokio::spawn` for background indexing at startup.

## Feature Flags

| Crate           | Flag            | Effect                                                |
|-----------------|-----------------|-------------------------------------------------------|
| `wingman-rag`   | `embeddings`    | Enable fastembed; disable to use hash fallback.       |
| `wingman-rag`   | `treesitter`    | Enable semantic chunking; disable for line-window.    |
| `wingman-ts`    | `treesitter`    | Enable tree-sitter parsing; disable for No-op.        |
| `wingman-ts`    | `highlight`     | Enable syntax highlighting (tree-sitter-highlight).  |
| `wingman-learn` | `treesitter`    | Enable tree-sitter in learning hooks.                 |

## Error Handling

- Most fallible operations return `Result<T>` with a custom `Error` type per crate.
- Agent loop continues on tool execution errors (error message added to history).
- Config load errors are fatal.
- Missing optional features (no tree-sitter) degrade gracefully (empty Vec/None).

## Performance Considerations

- **Token estimation:** `estimate_tokens()` for history; used to trigger compaction.
- **Compaction:** old turns summarized and replaced when history exceeds threshold.
- **RAG embeddings:** background task at startup; cached in SQLite.
- **Session embedding:** deferred (not blocking agent loop); backfilled on next startup.
- **Memory index:** read once at startup; ~100 bytes per memory (list view), fetched full on use.
- **Tool output truncation:** head/tail per `tool_output_max_lines` to prevent token overflow.
