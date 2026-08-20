# Feature Reference

The complete feature list. The [README](../README.md) covers what makes
Wingman different; this is everything else it does.

- **Persistent memory and skills.** Memories are plain markdown +
  frontmatter under `~/.wingman/memory/` and `<project>/.wingman/memory/` —
  files you can read, edit, and delete, not an opaque store. Plus skill usage
  stats, cross-session semantic recall through the RAG pipeline, and
  quiet-session nudges to persist something worth keeping. Note the outcome
  "scoring" behind skill stats is a phrase heuristic over your replies, not a
  learned signal; treat the numbers as a rough tally. See
  [LEARNING-LOOP.md](LEARNING-LOOP.md).
- **73+ providers, one shape.** Anthropic is the reference implementation
  (streaming, tool use, explicit prompt caching). A single OpenAI-compatible
  adapter covers OpenAI, OpenRouter, LM Studio, vLLM, LiteLLM, and Ollama.
  Gemini and ChatGPT (OAuth) have their own adapters. All speak the same
  `wingman_core::Message` contract.
- **Three surfaces.** A `ratatui`-based TUI for interactive coding, a
  headless `--print` mode that emits either text or newline-delimited JSON
  events, and a `--batch <file.jsonl>` mode that runs a file of prompts
  non-interactively — all ready to pipe into other tools or CI.
- **MCP host.** Declare Model Context Protocol servers under `[mcp.<name>]`
  in config (stdio or HTTP transport); their tools are namespaced as
  `mcp__<server>__<tool>` and dispatched like built-ins. Manage them live
  from the TUI with `/mcp`.
- **Guided provider login.** `wingman login <provider>` (or `/login` in the
  TUI) probes the key, stores it in the OS keyring, and records the default
  model; `wingman logout <provider>` clears it. ChatGPT uses a browser
  OAuth flow.
- **Multi-agent pilot mode.** `wingman pilot run "<goal>"` plans, spawns
  worker agents in isolated worktrees, and opens a PR. See
  [PILOT-MODE.md](PILOT-MODE.md).
- **`wingman knows`.** Prints what Wingman knows about the current project:
  memories, skills, model routing, the verification gate, and index
  freshness.
- **Built-in tool layer.** File read/write/edit, glob, grep, directory
  listing, shell execution, semantic search, and the new learning tools
  (`save_memory`, `recall_memory`, `invoke_skill`, `recall_session`,
  `read_session`), each gated by the active permission mode.
- **Live model swap.** Change provider/model mid-session with `/model
  <provider>/<id>` from inside the TUI — no restart, history preserved.
- **Portable reasoning control.** One `reasoning = off|low|medium|high` level
  (`--reasoning`, `WINGMAN_REASONING`, or `/reasoning` live in the TUI) maps
  onto Anthropic's thinking budget, OpenAI's `reasoning_effort`, and Gemini's
  `thinkingConfig`. Reasoning streams to the UI dimmed and collapses to one
  line once the answer starts; on Anthropic it round-trips through history
  with its signature intact so multi-turn tool use keeps working. Off by
  default — thinking tokens bill at the output rate. Backends without a
  reasoning control ignore it, and `wingman doctor` names them.
- **Concurrent reads.** When a turn's tool calls are all pure reads
  (`read_file`, `grep`, `lsp_references`, …) they dispatch together instead of
  one at a time. A batch containing an edit or a shell command stays
  sequential, because those calls have to see the tree the previous one left
  behind. Results always come back in the order the model asked for them.
- **Token-aware pipeline.** Per-tool output budgets with head/tail
  truncation, history token estimation, and a compaction trigger
  (`compact_at_tokens`) so long sessions stay inside the active model's
  context window.
- **Layered configuration.** Defaults → global `~/.wingman/config.toml` →
  project `.wingman/config.toml` → `WINGMAN_*` env vars → CLI flags. TOML
  sub-tables merge instead of clobbering.
- **Permission modes.** `read-only` (default), `plan` (read-only until you
  `/approve` the agent's plan, then auto-edit), `auto-edit` (writes
  inside the project tree and shell auto-allowed, subject to the shell
  denylist), and `yolo` (no guardrails; per-session only, never persisted).
  Modes are enforced centrally: each tool declares what it needs
  (read / write / shell / network) and the registry refuses anything the
  active mode doesn't grant. A few paths — `.git/`, `.wingman/config.toml`,
  `.wingman/skills/` — are never writable, in any mode. `run_shell` is
  additionally confined by the OS where possible (`bwrap` / `sandbox-exec`,
  Job Object on Windows — which contains the process but not its file access);
  see [CONFIGURATION.md](CONFIGURATION.md#permission-modes).
- **Untrusted project config.** A cloned repo's `.wingman/config.toml` may
  pick a model and tune the UI, but not run commands: `[hooks]`, `[mcp]`,
  `[verify]`, `[providers]`, and `permission_mode` are ignored unless you
  run `wingman trust` in that repo. Trust is pinned to the file's contents
  and lapses whenever it changes.
- **Lifecycle hooks.** `pre_tool_use`, `post_tool_use`, `user_prompt_submit`,
  and `stop` shell hooks (`[hooks]` in config). A hook with `block = true`
  that exits non-zero refuses the tool call (`pre_tool_use`) or the prompt
  (`user_prompt_submit`); `stop` is advisory, since the turn is already
  over. Hook failures are always logged, whether or not they block.
- **Web tools.** Built-in `web_fetch` (URL → text) and `web_search`
  (DuckDuckGo HTML, no API key) tools pair for "look something up".
- **Atomic multi-file patches.** The `apply_patch` tool applies a
  multi-file edit block atomically — no partial writes on failure.
- **Working-tree checkpoints.** `wingman checkpoint` snapshots the tree
  into a tagged `git stash`; `wingman undo` restores the most recent one.
- **`wingman init`.** Scans the project (Cargo.toml, package.json,
  pyproject.toml, go.mod, …) and writes a starter `WINGMAN.md`.
- **`wingman cost`.** Per-model token + USD spend table derived from
  `~/.wingman/usage.json` and `pricing.rs`.
- **`wingman session list / fork`.** Browse recent session JSONLs;
  fork an old session (optionally truncating to N records) and resume it.
- **User-defined slash commands.** Drop a markdown file at
  `~/.wingman/commands/<name>.md` (or `<project>/.wingman/commands/`) and
  it becomes `/<name>` in the TUI. `$ARGS` is substituted.
- **In-transcript search.** `/find <query>`, `/findnext`, `/findprev`,
  `/findclear` walk hits inside the current transcript. Mouse wheel
  scrolling is enabled.
- **File-tree sidebar.** `Ctrl+B` toggles a left-side file browser; `j`/`k`
  move, `Tab` descends, `Enter` inserts the path into the composer.
- **Themes.** `tui.theme = "default" | "light" | "mono"`, plus optional
  per-role color overrides under `tui.colors` (`"#rrggbb"` hex or named).
- **Model fallback.** `router.fallback_models = ["openai/gpt-4.1",
  "openrouter/anthropic/claude-opus-4-7"]` — on primary failure the
  runtime walks the chain in order.
- **Subagent tool.** The model can call `spawn_subagent` to run an
  isolated inner agent loop on a focused sub-task (depth-capped at 1).
- **Notebook reads.** `read_file` on a `.ipynb` returns cells as fenced
  code blocks + markdown, not raw JSON.
- **Scheduled tasks.** `[[schedule]]` config entries fire from
  `wingman schedule` (call from cron / Task Scheduler).
- **Memory packs.** `wingman memory export/import/diff` for sharing
  team-level memory.
- **Worktree sandbox.** `wingman worktree create <branch>` spins up an
  isolated working copy under `.wingman/worktrees/`.
- **PR review.** `wingman review <pr#>` (or `--local <base>`) runs a
  one-shot review prompt against the diff.
- **Local model auto-discovery.** `wingman discover` probes localhost
  Ollama / LM Studio / vLLM and prints available models.
- **Skill auto-extraction.** `wingman skill extract` scans recent session
  JSONLs for repeated tool-call sequences (e.g. `grep_tool → read_file →
  edit_file`) and writes draft skill markdown files under
  `~/.wingman/skills/proposed/` for you to review.
- **Tree-sitter powered code understanding.** Deep language-aware parsing
  (Rust, Python, JavaScript, TypeScript, Go) for semantic chunking in the RAG
  index, symbol extraction, AST-aware diffs, and outline generation. Feature-gated
  so the workspace builds without the C toolchain if you don't need parsing.
- **LSP-backed code intelligence.** Real, *resolved* go-to-definition,
  find-references, hover, diagnostics, and project-wide rename via whatever
  language server you have on `PATH` (rust-analyzer, pyright/pylsp,
  typescript-language-server, gopls) — the semantic upgrade over the
  tree-sitter heuristics. Tools `lsp_definition`, `lsp_references`, `lsp_hover`,
  `lsp_diagnostics`, `lsp_rename` degrade gracefully to the heuristic tools when
  no server is installed. See [LSP.md](LSP.md).
- **LSP-backed verification receipts.** The post-edit turn gate can fold the
  language server's diagnostics for the *changed* files into the verdict
  (`[verify].lsp_diagnostics`), so a change that introduces a type error the
  compile step missed fails verification: `✓ builds  ✓ affected tests  ✓ 0 new
  LSP diagnostics`.
- **Git-backed team memory.** `wingman memory sync [<git-ref>]` reconciles the
  team-shared `<project>/.wingman/memory/` — rebuilds the `MEMORY.md` index from
  the files on disk (resolving the "two teammates both added a memory" merge
  conflict) and optionally folds in memory files from a git ref without
  clobbering local ones.
- **Provider-cost arbitrage.** `wingman cost --compare` reprices your actual
  token volume against a spread of models (Opus / Sonnet / Haiku / GPT-5 /
  Gemini / DeepSeek) — what the same work would have cost elsewhere. Only a
  provider-agnostic agent can show this.
- **Portable skill interop.** `wingman skill import <path>` /
  `wingman skill export <name> <dir>` bridge wingman skills and the
  ecosystem-standard `SKILL.md` format (Claude Code, Codex, Cursor, Gemini CLI,
  Copilot, Cline, Goose).
- **LSP code-actions.** The `lsp_code_action` tool lists and applies the
  language server's *own* canonical fixes — add missing import, implement trait,
  fix lint, and `organize_imports` — instead of hand-editing.
- **Wingman as an MCP server.** `wingman mcp-serve` exposes Wingman's tools over
  MCP stdio so any MCP client (Claude Code, Cursor, another Wingman) can consume
  them — most valuably `semantic_search` (the warm repo index) and
  `recall_memory` (team memory). Read-only by default. Wingman is both an MCP
  host *and* an MCP server.
- **HTTP/SSE API.** `wingman serve` puts one daemon in front of an allowlist of
  repos so another machine, a phone, a Shortcut, or CI can drive Wingman: run
  and steer pilot fleets, hold streaming conversations, and reach the rest of
  the CLI. Bearer auth, and a permission ceiling a request cannot raise.
  `--remote <url>` points the CLI at it. See [HTTP-API.md](HTTP-API.md).
- **Git-native auto-commit.** `[git].auto_commit = true` turns each AI change
  into a reviewable, revertable commit with a generated message (Aider-style),
  composing with the rewind timeline and verification gate.
- **Local-first privacy preset.** `wingman router preset local` prints a
  `[router.classes]` block that points the cheap task classes at a local
  model. Caveat worth knowing: compaction and commit messages are currently
  computed without a model call at all, and `[router.classes]` is consulted
  only for subagents — so today the preset is a starting point for your own
  config rather than a switch that redirects live traffic. For a real
  guarantee use `[privacy].local_only` and `wingman attest`.
- **Explain-and-teach.** `wingman explain` gives a per-file "what changed and
  why it matters" walkthrough of the working diff (fast-model), for reviewers
  and juniors.
- **Audit trail.** `[audit].enabled = true` appends a JSONL record (timestamp,
  tool, redacted input, error flag) for every tool call — a compliance trail
  for teams.
- **Benchmark harness.** `wingman bench` runs a suite of prompts and records
  time-to-first-token, tokens/task, and verified-done rate.
- **Embeddable.** Use `wingman-core` as a library or drive Wingman from any
  language over MCP (`wingman mcp-serve`). See [SDK.md](SDK.md).
- **Visual verification.** *(Opt-in build.)* Build with `--features browser`
  and set `[verify.browser].url` to make the turn gate load a URL, screenshot
  it, and fail if it drifts from a baseline. Not in the default build, and it
  fails open — with no browser present the gate passes rather than blocking.
- **Server-backed team memory.** Beyond the git-backed `memory sync`,
  `wingman memory push` / `pull` sync memories through a team HTTP endpoint
  (`[team]`), merging non-destructively.
- **Multi-channel pilot intake.** `wingman pilot intake slack | email`
  turns Slack events or delivered `.eml` files into pilot requests.
- **VS Code extension.** `editors/vscode` brings `semantic_search` and
  `recall_memory` into the editor over `wingman mcp-serve`.
- **Agent Client Protocol.** `wingman acp` speaks ACP over stdio, so Zed,
  JetBrains, Neovim, and Emacs can drive Wingman as their agent — one protocol
  instead of a plugin per editor. The editor can decline an individual tool
  call (`session/request_permission`) and serve file reads from its unsaved
  buffers (`fs/read_text_file`); both sit *on top of* Wingman’s own permission
  mode rather than replacing it, so a client can narrow what the agent may do
  but never widen it. Writes still go to disk through Wingman
  ([#127](https://github.com/vedantnimbarte/Wingman/issues/127)).
- **Hybrid semantic search.** The index fuses dense vector similarity with BM25
  keyword scoring (reciprocal-rank fusion), so exact identifier/error-string
  matches surface alongside semantic ones.
- **Secret-scanned tool output.** High-confidence tokens (OpenAI/GitHub/AWS/
  Slack/JWT/PEM) are redacted from tool output before the model sees them
  (`[tools].redact_output_secrets`).
- **Custom command tools.** Define a tool as a shell command under
  `[[tools.custom]]` — extend the agent without recompiling.
- **`wingman doctor`.** One health check for config, credentials, local servers,
  the index, and language servers on PATH.
- **`wingman memory review`.** Promote or discard the facts `wingman distill`
  proposes — the review queue that closes the learning loop.
- **11 LSP languages.** Rust, Python, JS/TS, Go, Java, C/C++, Ruby, C#, PHP.
- **Session cost budget.** `[tokens].max_usd_per_session` warns when a session's
  estimated spend crosses your limit.
- **Characterization / golden testing.** `wingman golden capture/check` snapshots
  a command's output and the verification gate (`[verify].golden`) fails on any
  drift — a regression net for undertested/legacy code ("verified correct, not
  just verified builds").
- **Ask, don't guess.** The `ask_user` tool lets the agent pause and ask at a
  genuine fork or before an irreversible action instead of guessing.
- **Air-gapped mode.** `[privacy].local_only` refuses any non-local provider
  and removes the network tools. `wingman attest` audits every configured
  egress channel — MCP servers, hooks, custom tools, team endpoint, and
  whether `run_shell` is reachable — and states its own scope: it reflects
  configuration, and cannot vouch for what a local model or a spawned process
  does with the data.
- **Cited memory.** `recall_memory` returns provenance (source + date) and the
  agent cites the memory it acts on.
- **Test-first.** `wingman spec "<intent>"` writes failing tests, then
  implements against them. The `[verify]` gate pushes back on a red build for
  up to `[verify].max_retries` forced corrections (default 2), then stops and
  exits non-zero — bounded retries, not a loop until green.
- **PR-native.** `wingman pr address <pr#>` addresses a PR's review comments and
  failing CI on the current branch.
- **Repo onboarding.** `wingman tour` orients you on an unfamiliar codebase.
- **Preview & replay.** `wingman --print --dry-run` shows what it *would* do
  without changing anything; `wingman session replay <file>` re-runs a past
  session's prompts to reproduce it.
- **Multi-model code review.** `wingman review-multi <pr#> --models
  anthropic/claude-opus-4-7,openai/gpt-4.1,gemini/gemini-2.5-pro` fans the
  review out across reviewers in parallel and merges findings by
  file:line, marking which ones each reviewer raised.
- **Interactive hunk review.** `wingman diff <file>` walks each hunk of
  the working-tree diff one at a time with `[a]ccept / [r]eject / [s]kip
  / [q]uit`, then writes the merged result. Also accepts `--patch
  <file.patch>` for an arbitrary unified diff.
