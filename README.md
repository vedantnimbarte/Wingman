# Arc-Code

`arccode` is a multi-provider, terminal-first **self-improving** coding agent
written in Rust. It runs as a TUI for interactive sessions and as a headless
one-shot (`--print "prompt"`) for scripting, talks to nine LLM providers behind
a single streaming interface, ships a built-in tool layer for reading,
searching, and editing the project tree, and learns from every conversation:
it builds a persistent model of you and your projects, creates and refines
skills from observed work, and recalls past sessions across projects.

It is positioned as an open, provider-agnostic alternative to Claude Code,
Cursor, and Aider — with native support for Anthropic, OpenAI, ChatGPT
(OAuth), Google Gemini, OpenRouter, LiteLLM, LM Studio, vLLM, and Ollama,
plus a planned MCP host.

---

## Highlights

- **Self-improving learning loop.** Persistent memories (markdown +
  frontmatter under `~/.arccode/memory/` and `<project>/.arccode/memory/`),
  skill usage stats with outcome scoring, cross-session semantic recall via
  the existing RAG pipeline, and quiet-session nudges that ask the agent to
  consider persisting something when it's been a while since a save. See
  [Self-improving loop](#self-improving-loop) below.
- **Nine providers, one shape.** Anthropic is the reference implementation
  (streaming, tool use, explicit prompt caching). A single OpenAI-compatible
  adapter covers OpenAI, OpenRouter, LM Studio, vLLM, LiteLLM, and Ollama.
  Gemini and ChatGPT (OAuth) have their own adapters. All speak the same
  `arccode_core::Message` contract.
- **Two surfaces.** A `ratatui`-based TUI for interactive coding and a
  headless `--print` mode that emits either text or newline-delimited JSON
  events — ready to pipe into other tools or CI.
- **Built-in tool layer.** File read/write/edit, glob, grep, directory
  listing, shell execution, semantic search, and the new learning tools
  (`save_memory`, `recall_memory`, `invoke_skill`, `recall_session`,
  `read_session`), each gated by the active permission mode.
- **Live model swap.** Change provider/model mid-session with `/model
  <provider>/<id>` from inside the TUI — no restart, history preserved.
- **Token-aware pipeline.** Per-tool output budgets with head/tail
  truncation, history token estimation, and a compaction trigger
  (`compact_at_tokens`) so long sessions stay inside the active model's
  context window.
- **Layered configuration.** Defaults → global `~/.arccode/config.toml` →
  project `.arccode/config.toml` → `ARCCODE_*` env vars → CLI flags. TOML
  sub-tables merge instead of clobbering.
- **Permission modes.** `read-only` (default), `auto-edit` (writes/shell
  inside the project tree auto-allowed, denylist still prompts), and `yolo`
  (no prompts; per-session only, never persisted).

---

## Workspace layout

This is a Cargo workspace. Each crate has a narrow, well-defined responsibility.

| Crate                | Role                                                                                                  |
| -------------------- | ----------------------------------------------------------------------------------------------------- |
| `arccode-cli`        | Binary entry point. Argument parsing, logging, runtime wiring, headless mode.                          |
| `arccode-core`       | Provider-agnostic types: `Message`, `ContentBlock`, `CompletionRequest`, `Provider`, agent loop, streaming events, tool dispatch, token estimation. |
| `arccode-config`     | TOML config loading, layered merge, env-var resolution, permission model.                              |
| `arccode-providers`  | Concrete `Provider` implementations: Anthropic, Gemini, OpenAI-compatible (six variants).              |
| `arccode-tools`      | Built-in tool implementations (`read_file`, `write_file`, `edit_file`, `glob`, `grep`, `list_dir`, `run_shell`) and the `ToolRegistry`. |
| `arccode-tui`        | `ratatui` interactive surface: composer, transcript, status bar, slash commands.                       |
| `arccode-session`    | Append-only JSONL session log + replay/reconstruction for `/resume`.                                   |
| `arccode-rag`        | SQLite-backed code index with `fastembed` (BGE small) or a deterministic hash embedder fallback.       |
| `arccode-skills`     | Markdown-frontmatter skill files (global + project), auto-loaded into the system prompt.               |
| `arccode-learn`      | Self-improving loop: persistent memory store, skill usage stats, session embedding/recall, agent hooks.|
| `arccode-mcp`        | MCP host scaffolding (M3).                                                                             |

---

## Supported providers

| Provider           | Adapter                  | Notes                                                                    |
| ------------------ | ------------------------ | ------------------------------------------------------------------------ |
| Anthropic          | `AnthropicProvider`      | Reference impl. Streaming, tool use, explicit `cache_control` breakpoints. |
| OpenAI             | `OpenAiCompatProvider`   | Variant: `OpenAi`.                                                        |
| ChatGPT (OAuth)    | `ChatGptProvider`        | Browser OAuth login via `/login`; token kept in the OS keychain.          |
| OpenRouter         | `OpenAiCompatProvider`   | Variant: `OpenRouter`. Aggregator — pass `provider/model` as model id.    |
| LiteLLM            | `OpenAiCompatProvider`   | Variant: `LiteLLM`. Self-hosted gateway.                                  |
| LM Studio          | `OpenAiCompatProvider`   | Variant: `LmStudio`. Local OpenAI-compatible shim.                        |
| vLLM               | `OpenAiCompatProvider`   | Variant: `Vllm`. Self-hosted inference server.                            |
| Ollama             | `OpenAiCompatProvider`   | Variant: `Ollama`. Hits `/v1` shim on localhost:11434.                    |
| Google Gemini      | `GeminiProvider`         | Native adapter.                                                          |

---

## Installation

### Prerequisites

- Rust 1.80 or later (uses 2021 edition; pinned in `Cargo.toml`).
- A working C toolchain for some transitive crates.
- (Optional) An API key for the provider(s) you intend to use.

### Build from source

```bash
git clone git@github.com:vedantnimbarte/Arc-Code.git
cd Arc-Code
cargo build --release
```

The resulting binary is at `target/release/arccode` (or `arccode.exe` on
Windows).

To install onto your `PATH`:

```bash
cargo install --path crates/arccode-cli
```

---

## Quick start

### 1. Scaffold a config

```bash
arccode config init
```

This writes a starter `~/.arccode/config.toml` populated with entries for every
supported provider, each pointing at a `${ENV_VAR}` placeholder for the API
key.

### 2. Set an API key

Pick one of the supported providers and export its key. Anthropic is the
default:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
# or
export OPENAI_API_KEY=sk-...
export OPENROUTER_API_KEY=...
export GOOGLE_API_KEY=...
```

For local providers (Ollama, LM Studio, vLLM) no key is needed — just point
the `base_url` at the running instance.

### 3. Use it

```bash
# Interactive TUI in the current project
arccode

# Headless one-shot
arccode --print "explain the agent loop in crates/arccode-core"

# Headless, streaming JSON events (newline-delimited)
arccode --print "list the public types in arccode-core" --json

# Pick a model for this session only
arccode --model anthropic/claude-opus-4-7
arccode --model openai/gpt-4.1
arccode --model gemini/gemini-2.5-pro
arccode --model openrouter/anthropic/claude-opus-4-7

# Loosen the permission model for this session
arccode --mode auto-edit
arccode --mode yolo            # no prompts; per-session only
```

### Inside the TUI

- Type a prompt and hit Enter to send.
- `/model <provider>/<model-id>` — swap the active model live.
- `/memory` — list saved memories. `/memory forget <name>` to delete one.
- `/recall <query>` — search across past sessions for prior context.
- `/skill stats [name]` — show skill usage and outcome counts.
- `/learn [status|reset]` — self-learning loop dashboard.
- Tool calls render inline with their output (head/tail truncated per the
  active budget) and the token-usage strip updates after each turn.

Tell the agent things like "remember that I prefer pnpm over npm" or "from
now on always run `cargo fmt` before commits" — it will call `save_memory`
and the next session will see it in the system prompt.

---

## Self-improving loop

Every session contributes to a small set of files under `~/.arccode/` and
`<project>/.arccode/` that subsequent runs read on startup. There is no
cloud component — everything is local-first.

### What's persisted

- **Memories** at `~/.arccode/memory/<slug>.md` (global) or
  `<project>/.arccode/memory/<slug>.md` (project), indexed by a sibling
  `MEMORY.md`. Each memory is markdown with YAML frontmatter
  (`name`, `description`, `type`). Four types:

  | Type        | Default scope | Used for                                                       |
  | ----------- | ------------- | -------------------------------------------------------------- |
  | `user`      | global        | Facts about the human (role, expertise, working style).        |
  | `feedback`  | global        | How to behave (terse responses, avoid mocks, etc.).            |
  | `project`   | project       | Facts about this codebase (build commands, conventions).       |
  | `reference` | global        | Pointers to external systems (issue tracker, dashboards).      |

  The memory **index** (one bullet per memory) is rendered into the system
  prompt every turn. Full bodies stay on disk; the agent fetches them via
  `recall_memory` when relevant.

- **Skill usage** at `~/.arccode/learn.db` (SQLite). Every `invoke_skill`
  call is recorded; the next user turn flips its outcome to `success` or
  `corrected` based on negation heuristics ("no,", "wait,", "wrong,",
  "actually,"…). When a skill crosses 3 invocations with ≥50% correction
  rate, the next session's system prompt suggests a rewrite.

- **Session embeddings** at `~/.arccode/sessions.db`. Finished session
  JSONLs are chunked into thread-shaped windows and embedded using the
  same `fastembed`/hash backend that powers `semantic_search`. The CLI
  backfills any unindexed sessions in the background at startup.

### Learning tools (callable by the agent)

| Tool             | When the agent uses it                                                          |
| ---------------- | ------------------------------------------------------------------------------- |
| `save_memory`    | User says "remember", "from now on", expresses a stable preference.             |
| `recall_memory`  | The memory index in the prompt hints at relevance and the agent needs the body.|
| `forget_memory`  | User explicitly asks to forget, or a memory is clearly wrong.                   |
| `invoke_skill`   | A skill from the catalog matches the task; instructions apply for the turn.    |
| `recall_session` | "Have we discussed X before?" / "How did we fix Y last time?"                   |
| `read_session`   | Drill into a specific session id returned by `recall_session`.                  |

### Nudges

After a configurable number of quiet sessions (default 5 — no saves), the
system prompt for the next turn includes a one-line nudge asking the agent
to consider proposing a memory if anything surprising came up. `/learn
reset` zeros the counter; `/learn status` shows where you stand.

---

## Configuration

`arccode` resolves configuration in this order (lowest to highest precedence):

1. Built-in defaults.
2. `~/.arccode/config.toml` (global).
3. `<project>/.arccode/config.toml` (project-local).
4. `ARCCODE_*` environment variables.
5. CLI flags.

TOML sub-tables are merged at the raw-TOML level, so an absent section in the
project file does **not** wipe out the global values for that section.

### Example `~/.arccode/config.toml`

```toml
default_provider = "anthropic"
permission_mode = "read-only"

[tokens]
compact_at_tokens = 120000
tool_output_max_lines = 400
prompt_cache = true

[router]
fast_model = "anthropic/claude-haiku-4-5-20251001"

[tui]
theme = "default"
show_token_usage = true

[providers.anthropic]
api_key = "${ANTHROPIC_API_KEY}"
model = "claude-opus-4-7"

[providers.openai]
api_key = "${OPENAI_API_KEY}"
model = "gpt-4.1"

[providers.gemini]
api_key = "${GOOGLE_API_KEY}"
model = "gemini-2.5-pro"

[providers.openrouter]
api_key = "${OPENROUTER_API_KEY}"
model = "anthropic/claude-opus-4-7"

[providers.ollama]
base_url = "http://localhost:11434/v1"
model = "llama3.1:8b"

[providers.lmstudio]
base_url = "http://localhost:1234/v1"
model = "local-model"

[providers.vllm]
base_url = "http://localhost:8000/v1"
model = "local-model"

[providers.litellm]
api_key = "${LITELLM_API_KEY}"
base_url = "http://localhost:4000/v1"
model = "anthropic/claude-opus-4-7"

[logging]
filter = "info,arccode=info"
file = true
```

### Environment variables

| Variable                            | Effect                                                              |
| ----------------------------------- | ------------------------------------------------------------------- |
| `ARCCODE_MODEL`                     | Overrides `default_model`. Same syntax as `--model`.                |
| `ARCCODE_PROVIDER`                  | Overrides `default_provider`.                                       |
| `ARCCODE_PERMISSION_MODE`           | `read-only` \| `auto-edit` \| `yolo`.                               |
| `ARCCODE_LOG`                       | `tracing-subscriber` env-filter directive.                          |
| `ARCCODE_<PROVIDER>_API_KEY`        | Sets `providers.<provider>.api_key`.                                |
| `ARCCODE_<PROVIDER>_BASE_URL`       | Sets `providers.<provider>.base_url`.                               |
| `ARCCODE_<PROVIDER>_MODEL`          | Sets `providers.<provider>.model`.                                  |

Any string field of the form `${ENV_VAR}` (e.g. `api_key = "${ANTHROPIC_API_KEY}"`)
is resolved against the environment at load time.

### Permission modes

| Mode         | Reads / Search | Writes inside project | Shell                       | Out-of-tree paths |
| ------------ | -------------- | --------------------- | --------------------------- | ----------------- |
| `read-only`  | allowed        | prompts               | prompts                     | prompts           |
| `auto-edit`  | allowed        | auto-allowed          | auto-allowed except denylist | prompts           |
| `yolo`       | allowed        | auto-allowed          | auto-allowed                | auto-allowed      |

`yolo` is per-session only — never persisted to config.

---

## CLI reference

```text
arccode [OPTIONS] [COMMAND]
```

**Top-level flags**

| Flag                     | Description                                                                 |
| ------------------------ | --------------------------------------------------------------------------- |
| `--mode <MODE>`          | `read-only` \| `auto-edit` \| `yolo`.                                       |
| `--model <MODEL>`        | Model id, optionally prefixed: `anthropic/claude-opus-4-7`. Env: `ARCCODE_MODEL`. |
| `--print <PROMPT>`       | Run a single prompt and exit (non-interactive).                              |
| `--json`                 | Emit newline-delimited JSON events instead of text. Use with `--print`.      |
| `-v`, `-vv`              | Increase log verbosity.                                                      |
| `--quiet`                | Suppress non-error stderr output.                                            |
| `--version`              | Print version and exit.                                                      |
| `--help`                 | Print help.                                                                  |

**Subcommands**

| Command              | Description                                            |
| -------------------- | ------------------------------------------------------ |
| `config init`        | Write a starter `~/.arccode/config.toml`. `--force` to overwrite. |
| `config show`        | Print the merged effective configuration. `--json` for JSON output. |
| `config paths`       | Print the resolved global and project config paths.    |

Running `arccode` with no subcommand launches the TUI against the resolved
provider and model.

---

## Built-in tools

Each tool runs through the registry, which receives a `ToolCtx` carrying the
active permission mode, current working directory, and project root. Tools
decide whether to act, prompt, or refuse based on that context.

| Tool              | Purpose                                                                 |
| ----------------- | ----------------------------------------------------------------------- |
| `read_file`       | Read a file by absolute path. Returns content with line numbers.        |
| `write_file`      | Create or overwrite a file.                                             |
| `edit_file`       | Exact string replacement inside an existing file.                       |
| `glob_tool`       | Find files by glob pattern (e.g. `**/*.rs`).                            |
| `grep_tool`       | Content search via ripgrep semantics.                                   |
| `list_dir`        | List a directory.                                                       |
| `run_shell`       | Execute a shell command. Subject to the permission denylist.            |
| `semantic_search` | Cosine search the project RAG index for relevant code chunks.           |
| `save_memory`     | Persist a fact / preference / instruction across sessions.              |
| `recall_memory`   | Read the full body of a memory by slug.                                 |
| `forget_memory`   | Delete a memory by slug.                                                |
| `invoke_skill`    | Load a named skill's body for the current turn; records into stats.     |
| `recall_session`  | Cross-project semantic search over past session transcripts.            |
| `read_session`    | Fetch a full session JSONL by id.                                       |

Tool output is bounded by `tokens.tool_output_max_lines`; anything longer is
head/tail truncated before being fed back into the model.

---

## Roadmap

The project is being built milestone by milestone:

- **M0** — Workspace scaffold, CLI surface, layered config loader. *(shipped)*
- **M1** — Headless and TUI agent loop against Anthropic with built-in tools. *(shipped)*
- **M2** — Six more providers, token pipeline, live `/model` swap. *(shipped)*
- **M3** — Session persistence (`arccode-session`), `/resume`, MCP host
  scaffolding (`arccode-mcp`). *(shipped — session persistence; MCP host in
  progress)*
- **M4** — Repo index / RAG (`arccode-rag`) with SQLite store and `fastembed`
  or hash-embedder fallback. *(shipped)*
- **M5** — Skills (`arccode-skills`), ChatGPT OAuth, TUI polish (welcome
  screen, slash autocomplete). *(shipped)*
- **M6** — Self-improving learning loop (`arccode-learn`): persistent
  memories, skill usage stats with outcome scoring, cross-session recall,
  nudges. *(shipped — current `main`)*
- **Next** — Interactive TUI approval modal for skill/memory proposals,
  session logging from the TUI (currently headless-only), full MCP host.

---

## Development

### Build & test

```bash
cargo build              # debug build
cargo build --release    # release build
cargo test               # full test suite
cargo fmt                # formatting (rustfmt.toml is project-pinned)
cargo clippy             # lints
```

### Run the TUI from source

```bash
cargo run -- --mode auto-edit
```

### Run a headless one-shot from source

```bash
cargo run -- --print "what does crates/arccode-core do?"
```

### Logs

By default, logs are written to `~/.arccode/logs/`. Override with
`ARCCODE_LOG=debug` or via the `[logging]` block in config.

---

## Project layout on disk

```
.
├── Cargo.toml              # workspace manifest
├── Cargo.lock
├── rustfmt.toml
├── crates/
│   ├── arccode-cli/        # binary entry point
│   ├── arccode-config/     # config loading + merge
│   ├── arccode-core/       # provider-agnostic types + agent loop + LearningHook
│   ├── arccode-learn/      # memory, skill stats, session recall, hooks
│   ├── arccode-mcp/        # MCP host (M3)
│   ├── arccode-providers/  # Anthropic, ChatGPT, Gemini, OpenAI-compat
│   ├── arccode-rag/        # repo + session index (SQLite + fastembed/hash)
│   ├── arccode-session/    # JSONL session log + replay
│   ├── arccode-skills/     # markdown-frontmatter skills loader
│   ├── arccode-tools/      # built-in tools + registry
│   └── arccode-tui/        # ratatui surface
└── target/                 # build output (gitignored)
```

On the user's machine:

```
~/.arccode/
├── config.toml             # global config
├── credentials.toml        # provider credentials (optional)
├── logs/                   # tracing output
├── skills/                 # global skills (*.md)
├── memory/                 # global memories
│   ├── MEMORY.md           #   index — one bullet per memory
│   └── <slug>.md           #   per-memory body
├── learn.db                # skill usage + outcome stats (SQLite)
└── sessions.db             # cross-project session embeddings (SQLite)
```

```
<project-root>/.arccode/
├── config.toml             # project-local overrides
├── sessions/               # per-session JSONL logs (append-only)
├── index.db                # project RAG index (SQLite + embeddings)
├── skills/                 # project-scoped skills (override globals by name)
└── memory/                 # project-scoped memories
    ├── MEMORY.md
    └── <slug>.md
```

---

## License

Dual-licensed under either:

- MIT License
- Apache License, Version 2.0

at your option.

---

## Contributing

Issues and pull requests are welcome. Before opening a PR:

1. `cargo fmt` and `cargo clippy` cleanly.
2. `cargo test` passes.
3. New behavior is covered by a test where reasonable.
