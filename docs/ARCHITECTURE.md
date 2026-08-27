# Wingman Architecture Overview

Wingman is a modular, multi-provider coding agent written in Rust. This document describes its high-level architecture, core abstractions, and how subsystems interact.

> Diagrams — the crate graph, one turn, a pilot run, a remote turn, and
> config/permission resolution — are in the
> [How it works](../README.md#how-it-works) section of the README. This
> document is the prose behind them.

## System Overview

Wingman operates on four surfaces:
1. **Interactive TUI** (`ratatui`-based) for long-lived sessions with state persistence and live interaction.
2. **Headless mode** (`--print`) for one-shot prompts that emit text or newline-delimited JSON events.
3. **`wingman serve`** — an HTTP/SSE API and web panel over an allowlist of repos, for driving Wingman from another machine, a phone, or CI.
4. **Pilot mode** (`wingman pilot`, `wingman board`) — multi-task runs whose workers are themselves `wingman` processes in isolated git worktrees.

All four feed the same **agent loop** at `crates/wingman-core/src/agent.rs`, which orchestrates:
- **Provider abstraction** — speak to nine LLM backends (Anthropic, OpenAI, Gemini, etc.) through a unified `Provider` trait.
- **Tool dispatch** — route model tool calls to built-in tools (file I/O, shell, search, code intelligence, memory, skills, etc.), concurrently when every call in the batch is a pure read.
- **Token management** — track token usage, estimate context, prune oversized tool results, compact history when needed.
- **Verification** — run a post-edit gate (build, affected tests, LSP diagnostics) before a turn that touched the tree may end.
- **Learning hooks** — persist memories, track skill usage, embed session transcripts for recall.
- **Context recording** — emit a `ContextFact` at every point the model's context changes, so the session log reconstructs exactly what was sent.

## Core Crates

### `wingman-cli`
**Purpose:** Binary entry point, argument parsing, logging setup, surface selection.

**Key files:**
- `src/main.rs` — entry point, error handling.
- `src/cli.rs` — clap argument structure.
- `src/commands/` — subcommand handlers (config, doctor, session, worktree, memory, review, cost, router, pilot, board, schedule, trust, …).
- `src/commands/headless.rs` — runs `--print` mode.
- `src/commands/worker.rs` — runs `--worker-mode`, the per-task agent a pilot run spawns.
- `src/runtime.rs` — builds the provider, registry, learning hook, and verification gate every surface shares.
- `src/serve/` — the HTTP/SSE daemon and the web panel it hosts.

**Responsibilities:**
- Parse CLI args and environment.
- Load global/project-local config via `wingman-config`.
- Select the surface (TUI, headless, worker, serve).
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
- `TurnGate` / `GateReport` — the post-edit verification gate (see below).
- `ContextSink` / `ContextFact` — the record of everything that reached the model.

**Agent loop flow** (one user turn; the sequence diagram is in the
[README](../README.md#one-turn)):
```
1. Record the prompt as a ContextFact, before anything can fail.
2. Under token pressure: prune oversized tool results in older turns, then
   compact the oldest span into a recap if still over budget.
3. LearningHook::before_turn — splice per-turn system text (memory index,
   retrieved chunks, nudges, skill bodies) onto the base prompt.
4. Build CompletionRequest (system, history, tool specs, cache breakpoints,
   reasoning effort) and call provider.complete().
5. Stream TextDelta / ThinkingDelta / ToolUse / Usage / Stop to the surface.
6. On tool calls: dispatch via the registry — concurrently when every call in
   the batch is on `parallel_safe_tools` and none is mutating, otherwise one
   at a time. Truncate each result to the ToolOutputBudget, spilling the full
   text where the model can still read it. A successful mutating call arms
   the gate. Loop.
7. On end_turn with the gate armed: run TurnGate::check(). Red and retries
   left → feed the report back as a user message and continue; red and
   retries spent → Stop::GateFailed (a non-zero exit, distinct from EndTurn).
8. LearningHook::after_stop, record Stop, yield it.
```

**Key abstractions:**
- `CacheBreakpoint` / `CacheKind` — Anthropic-style prompt caching for cost reduction. A breakpoint also rolls onto the last message each turn, so the growing conversation prefix is cached instead of re-billed.
- `Compactor` — when history token count exceeds `compact_at_tokens`, summarize old turns.
- `ToolResultPruner` — shrink oversized tool results in older turns *before* compaction folds whole turns away; the tokens are mostly in tool output while the value is mostly in what the assistant concluded from it.
- `ToolOutputBudget` — per-tool output size limits (head/tail truncation).
- `SpillStore` — where a truncated result's full text goes, so the cap stays a budget rather than a loss.
- `TurnGate` — runs only for user turns in which a mutating tool succeeded (`mutating_tools`, default `write_file`, `edit_file`, `apply_patch`, `edit_symbol`, `run_shell`, `lsp_rename`, `lsp_code_action`). Bounded by `gate_max_retries` (`[verify].max_retries`, default 2).
- `ReasoningEffort` — one `off|low|medium|high` knob mapped onto each provider's own reasoning control; backends without one ignore it.

### `wingman-config`
**Purpose:** Layered config resolution, permission model, hook system.

**Config resolution (ascending priority):**
1. Built-in defaults.
2. `~/.wingman/config.toml` (global).
3. `<project>/.wingman/config.toml` (project) — **split by trust**, see below.
4. `${ENV_VAR}` placeholder resolution.
5. `WINGMAN_*` environment variables.
6. CLI flags.

TOML sub-tables merge rather than clobber, so a project layer can set one key
of `[providers.anthropic]` without erasing the rest.

**The trust split.** A project's `.wingman/config.toml` is attacker-controlled
the moment you clone someone else's repository. Most of its keys are harmless
(which model, TUI preferences, token budgets) and always merge. The ones that
execute things — `[hooks]`, `[mcp]`, `[verify]`, `[[tools.custom]]`,
`permission_mode`, and everything under `[serve]` — merge only when the user
has run `wingman trust` on *that exact file content*. Trust is recorded in
`~/.wingman/trusted.toml` as a SHA-256 of the file bytes keyed by absolute
path, so editing a trusted config revokes trust until it is granted again.
Without that split, `git clone` would be code execution. See
`crates/wingman-config/src/trust.rs` and `PROJECT_SAFE_KEYS`.

**Key sections:**
- `[tokens]` — `compact_at_tokens`, `tool_output_max_lines`, `prompt_cache`.
- `[router]` — `default_provider`, `default_model`, `fast_model`, `fallback_models`.
- `[tui]` — `theme`, `show_token_usage`.
- `[providers.<name>]` — API key, base URL, model per provider.
- `[hooks]` — lifecycle hooks (pre_tool_use, post_tool_use, stop, user_prompt_submit).
- `[[schedule]]` — cron entries for recurring tasks.
- `[autonomous]` — limits, role overrides, branch prefix (planned M8).

**Permission modes:**
| Mode         | Reads | Writes (in-tree) | Shell | Network | Out-of-tree |
|--------------|-------|------------------|-------|---------|-------------|
| `read-only`  | allow | deny             | deny  | deny¹   | deny        |
| `plan`       | allow | deny until `/approve` | deny until `/approve` | deny¹ | deny |
| `auto-edit`  | allow | allow            | allow² | allow  | deny        |
| `yolo`       | allow | allow            | allow | allow   | allow       |

¹ unless `[tools].allow_network` is set. Network egress is an exfiltration
channel, so the read-only research modes can't reach it by default.
² subject to the built-in denylist plus `[tools].extra_denylist`.

**There are no prompts.** A call the mode doesn't grant is refused, and the
refusal goes back to the model as a tool error; nothing is queued for a human
to approve. Enforcement is one function — `capability_denial` in
`wingman-tools::registry` — checked against each tool's declared `Capability`
bits (`WRITE` / `SHELL` / `NETWORK`), so a tool that forgets to check its own
permissions fails closed rather than open. The mode itself lives in an atomic
cell shared by every clone of `ToolCtx`, which is how the TUI's `/mode` picker
re-gates a running agent without rebuilding the registry.

Two containments hold regardless of mode:
- **Reads are confined to the project tree** in every mode except `yolo`, so
  neither the agent nor prompt-injected text inside something it read can pull
  `~/.ssh/id_rsa` into tool output. Writes are confined the same way, with
  `..` components folded and symlinked parents resolved *before* the check —
  a repo can commit a dangling symlink pointing out of the tree, and it must
  not become a write primitive.
- **Protected paths are never writable, in any mode**: `.git/`,
  `.wingman/config.toml`, `.wingman/skills/`, and `.wingman/trusted.toml`.

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

**Tool registry (`ToolRegistry`):** implements `ToolDispatcher` for the agent
loop. One dispatch runs, in order:

1. `pre_tool_use` hooks — a blocking hook that fails turns the call into a tool
   error. A non-blocking hook that fails is still logged, because a policy hook
   whose binary is missing is a security control failing silently.
2. **Undo snapshot** — the pre-image of any file this call is about to mutate,
   committed only if the call succeeds, so `/undo` can restore it.
3. **Capability gate** — `capability_denial`, the central permission check.
4. **The tool**, under a backstop deadline (`[tools].tool_timeout_secs`); tools
   that bound themselves opt out via `Tool::owns_timeout`.
5. **Secret redaction** — high-confidence tokens stripped from the output
   before it reaches the model, so a credential the agent read can't be echoed
   back out.
6. **Audit trail** — an optional append-only JSONL record of what ran.
7. **Repeat guard** — identical consecutive calls get an advisory appended;
   counted *after* the capability gate, since a model hammering a denied call
   is exactly the loop worth breaking.
8. `post_tool_use` hooks — fire-and-forget, failures only logged.

The loop then truncates the result to the `ToolOutputBudget`, spilling the full
text where the model can still read it.

MCP tools register alongside the built-ins as `mcp__<server>__<tool>`, and the
registry is interior-mutable so servers can be added or removed at runtime from
behind the `Arc` the running agent holds.

**Built-in tools** (full reference: [TOOLS.md](TOOLS.md)):

| Group | Tools |
|-------|-------|
| Files | `read_file`, `write_file`, `edit_file`, `apply_patch`, `list_dir` |
| Search | `glob`, `grep`, `semantic_search` |
| Code intelligence — resolved | `lsp_definition`, `lsp_references`, `lsp_hover`, `lsp_rename`, `lsp_code_action`, `lsp_diagnostics` |
| Code intelligence — heuristic | `outline`, `find_symbol`, `who_calls`, `edit_symbol` |
| Shell | `run_shell`, `job_list`, `job_output`, `job_send`, `job_stop` |
| Network | `web_fetch`, `web_search` |
| Memory and skills | `save_memory`, `recall_memory`, `forget_memory`, `invoke_skill`, `recall_session`, `read_session` |
| Orchestration | `present_plan`, `run_plan`, `spawn_subagent`, `ask_user`, `update_tasks`, `task_complete` |

The heuristic group is tree-sitter name-matching; the `lsp_*` group asks a
language server and *resolves*. With no server installed for a language the
`lsp_*` tools say so and fall back to the heuristic equivalents rather than
failing.

<!-- The per-tool table that used to live here drifted from the code. Keep the
     groups above in sync with `crates/wingman-tools/src/builtin/`, and the
     detail in TOOLS.md. -->

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

**The invariant: model-visible means logged.**
(Decision record: [0002](decisions/0002-loop-owns-the-session-log.md).)

Everything that reaches a model request must be reconstructable from the log.
The agent loop is the only place that knows what actually went into a request,
so it is the only writer: at every point it changes what the model will see it
emits a `ContextFact` to a `ContextSink`, and `SessionLogSink` writes it down.
Surfaces open the log and hand it over; they no longer compose records
themselves.

They used to, and they disagreed. The TUI wrote the prompt text and the
streamed assistant text and nothing else — no tool calls, no tool results — so
resuming a TUI session rebuilt a conversation in which the agent had never used
a tool. Headless recorded more, `serve` recorded differently again.

Consequences worth knowing:

- A **truncated** tool result is logged twice over: `output` is the full text
  (the audit trail, and what the user was shown) and `model_output` is the
  bounded form actually sent. `records_to_messages` replays `model_output`,
  because reconstructing the conversation means reconstructing what the model
  received — not the richer thing the tool said.
- **Compaction** and **tool-result pruning** are recorded (`Recap`,
  `ToolResultPruned`) and replayed, so a resumed session is not silently longer
  than the one it continues.
- Per-turn **system-prompt injections** (memory recall, nudges, skill bodies)
  are recorded as `InjectedContext`. They are not message history and do not
  replay — the system prompt is rebuilt per turn — but a reader asking "why did
  it do that" can see them. `SessionStart.system_hash` pins the base prompt they
  were added to.
- Adding a new kind of model-visible input means adding a `ContextFact`. That is
  the point: it is hard to slip something into the model's context without also
  writing it down. A debug assertion (`debug_assert_reconstructs`) catches the
  case where someone does.

**Session format (one JSON object per line):**
```json
{"kind":"session_start","ts":"…","model":"…","provider":"…","system_hash":"…"}
{"kind":"user","ts":"…","text":"explain the agent loop"}
{"kind":"assistant","ts":"…","blocks":[{"type":"text","text":"Let me look…"},
                                       {"type":"tool_use","id":"t0","name":"read_file","input":{}}]}
{"kind":"tool_result","ts":"…","id":"t0","output":"<full>","model_output":"<bounded>","is_error":false}
{"kind":"recap","ts":"…","replaced":8,"text":"[wingman compact] …"}
{"kind":"stop","ts":"…","reason":"\"end_turn\""}
```

Old logs load unchanged: every addition is a new variant or a defaulted field.

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
- `semantic_search` tool (callable by agent) → **hybrid** retrieval: dense
  vector similarity fused with BM25 keyword scoring by reciprocal rank, so an
  exact identifier the embedding buries still surfaces.
- Session transcript embedding for `recall_session` cross-project search.
- A file watcher keeps the index fresh; `wingman doctor` reports when it is stale.

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
- Used by `wingman-rag` for semantic chunking, by `wingman-tools` for the heuristic symbol tools, and by the TUI for highlighting.
- Name-matching, not resolution. `wingman-lsp` is the semantic upgrade; these stay as the fallback when no language server is installed.

### `wingman-lsp`
**Purpose:** A Language Server Protocol client — resolved code intelligence
rather than grep.

- `server` — which server backs each language, and `PATH` detection.
- `client` — one live server connection over JSON-RPC/stdio.
- `LspManager` — lazily starts and pools one client per language, per workspace.

Eleven languages: Rust, Python, JavaScript, TypeScript, Go, Java, C, C++,
Ruby, C#, PHP — via whatever server the user has on `PATH` (rust-analyzer,
pyright/pylsp, typescript-language-server, gopls, …). Backs the `lsp_*` tools
and the diagnostics half of the verification gate. With no server present the
tools degrade to the `wingman-ts` heuristics instead of failing. See
[LSP.md](LSP.md).

### `wingman-mcp`
**Purpose:** MCP client integration — external tool servers as first-class
tools.

Connects to the servers declared in `[mcp]`, lists their tools, and adapts each
one to the registry as `mcp__<server>__<tool>` so it cannot collide with a
built-in or with another server. Both transports run over `rmcp`'s
`RunningService` and perform the MCP `initialize` handshake: stdio spawns a
child process, http uses the spec-compliant Streamable-HTTP client (SSE,
`Mcp-Session-Id`, auth headers). `[mcp]` is trust-gated, since a server entry
is a command to run.

MCP tools are deliberately absent from `parallel_safe_tools`: a stdio server
that mishandles concurrent requests is a bug we cannot reproduce from here.

### `wingman-autonomous`
**Purpose:** Pilot mode — planning a goal into a task DAG and running the tasks
as separate agents. The sequence is in the
[README](../README.md#a-pilot-run); details in [PILOT-MODE.md](PILOT-MODE.md).

| Module | Role |
|--------|------|
| `planner` | Goal + repo grounding facts → task DAG, via one LLM call plus a critique/rewrite loop. |
| `orchestrator` | The single-writer actor. Manager-agent tools send it commands over an mpsc channel and await an ack, so run state has exactly one mutator. |
| `store` | Append-only `tasks.jsonl` plus an atomically rewritten `state.json`, under `<project>/.wingman/autonomous/<run-id>/`. The log is the source of truth; disagreement resolves in its favour. |
| `worker` | Parent half of the worker subprocess: spawns `wingman --worker-mode … --print --json`, parses its NDJSON, forwards progress to the store, enforces `task_timeout_secs`. |
| `worktree` | One worktree and branch per task off the run's `base_commit`; integration merge is `git merge --squash` per task in topological order. |
| `review` / `critic` | Per-task reviewer on that task's diff, plus an always-on second model that critiques the plan, re-reviews each task, and can veto a merge. |
| `concurrency` | Adaptive cap from rate-limit headroom, host CPU load, and budget burn. |
| `pr` | `gh pr create` when `gh` is present and authenticated, otherwise push plus a compare URL — same `run.pr` event either way. |
| `daemon` | Goal discovery scoring (`value × confidence ÷ risk`) for autopilot. |
| `sandbox` | Per-task tier; the `vm` tier is fail-closed — pilot refuses those tasks rather than running them unsandboxed. |

### `wingman-board`
**Purpose:** A persistent kanban board over pilot runs, across every project.

Cards are goals you author and outlive the runs that execute them. Columns
(Backlog, Planned, In Progress, Review, Done) are **derived** from run state
rather than stored, so the board cannot disagree with `pilot watch` — both read
the same `state.json`. Card identity and dispatch history live in
`~/.wingman/board.db`; execution truth stays in the run store; the roll-up
table is a cache that is safe to delete. See [BOARD.md](BOARD.md).

### `wingman-browser`
**Purpose:** Headless-browser visual verification.

`diff_ratio` is a pure screenshot comparison and always compiles; `capture`
drives a headless Chrome and is behind the `chrome` feature (`browser` on
`wingman-cli`). The gate loads a URL, screenshots it, and fails if it differs
from a committed baseline by more than a threshold. It **fails open**: with no
browser present the gate passes rather than blocking. The feature links a
GPL-3.0 dependency, so a binary built with it is not redistributable under
Apache-2.0 — see [DEPENDENCIES.md](DEPENDENCIES.md).

## Data Flow Diagrams

The four main flows are drawn in the README's
[How it works](../README.md#how-it-works) section rather than duplicated here,
so there is one copy to keep true:

| Flow | Where |
|------|-------|
| Crate graph — surfaces, loop, providers, registry, disk | [Architecture](../README.md#architecture) |
| One turn, end to end, including the verification gate's retry path | [One turn](../README.md#one-turn) |
| A pilot run — plan, workers, review, squash merge, PR | [A pilot run](../README.md#a-pilot-run) |
| A remote turn over `wingman serve` | [A remote turn](../README.md#a-remote-turn) |
| Config layers, the trust split, the capability check | [Config and permissions](../README.md#config-and-permissions) |

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
    └─ Invocations and outcomes recorded in ~/.wingman/learn.db
```

## Threading Model

Wingman uses Tokio for async execution:

- **TUI:** spawns agent loop in a Tokio task, updates on event streams.
- **Headless:** single-threaded Tokio runtime, streams events to stdout/JSON.
- **Config/Session/Memory:** blocking I/O wrapped in `tokio::task::block_in_place`.
- **RAG embedding:** async `tokio::spawn` for background indexing at startup.
- **Tool dispatch:** a batch of pure reads runs concurrently via `buffered`,
  which preserves the model's ordering; anything else runs one at a time, so an
  edit or a shell command sees the workspace the previous call left behind.
- **Processes, not threads, at the outer edges.** Pilot workers and `serve`
  turns are child `wingman` processes. Both need their own current directory
  (project resolution is process-wide) and their own crash domain — a panicking
  turn ends one request, not the daemon.

## Feature Flags

| Crate           | Flag            | Effect                                                |
|-----------------|-----------------|-------------------------------------------------------|
| `wingman-cli`   | `embeddings`    | Default on. Real semantic search via fastembed/ONNX.  |
| `wingman-cli`   | `treesitter`    | Default on. Pulls the grammars through to every crate that uses them. |
| `wingman-cli`   | `browser`       | Off by default. The headless-browser verification gate; needs Chrome at runtime and links a GPL-3.0 dependency. |
| `wingman-rag`   | `embeddings`    | Enable fastembed; disable to use hash fallback.       |
| `wingman-rag`   | `treesitter`    | Enable semantic chunking; disable for line-window.    |
| `wingman-ts`    | `treesitter`    | Enable tree-sitter parsing; disable for No-op.        |
| `wingman-ts`    | `highlight`     | Enable syntax highlighting (tree-sitter-highlight).  |
| `wingman-tools` | `treesitter`    | Enable the heuristic symbol tools.                    |
| `wingman-learn` | `treesitter`    | Enable tree-sitter in learning hooks.                 |
| `wingman-browser` | `chrome`      | Compile screenshot capture; the diff logic is always compiled. |

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
- **Tool output truncation:** head/tail per `tool_output_max_lines` to prevent token overflow, with the elided middle spilled to disk so the model can still read it.
- **Tool-result pruning:** runs before compaction under token pressure — cheaper than folding away whole turns, and it keeps the reasoning while dropping the bulk.
- **Prompt caching:** a breakpoint rolls onto the last message each turn, so a multi-turn loop reads the cached conversation prefix instead of re-billing every prior turn.
- **Parallel reads:** a batch of pure reads runs concurrently; the model routinely emits four `read_file`s at once and serialising them is wall-clock spent for nothing.
