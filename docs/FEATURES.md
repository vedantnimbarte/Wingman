# Feature Reference

The complete feature list. The [README](../README.md) covers what makes
Wingman different; this is everything else it does.

- **Persistent memory and skills.** Memories are plain markdown +
  frontmatter under `~/.wingman/memory/` and `<project>/.wingman/memory/` —
  files you can read, edit, and delete, not an opaque store. Plus skill usage
  stats, cross-session semantic recall through the RAG pipeline, and
  quiet-session nudges to persist something worth keeping. Rate an answer with
  `/feedback good|bad [note]` and that rating scores the skill outright — the
  `rated=` column in `/skill stats` counts those. Anything unrated falls back
  to a phrase heuristic over your replies, which scores any non-correction as
  success, so read a high `ok=` with `rated=0` as a rough tally rather than a
  signal. See [LEARNING-LOOP.md](LEARNING-LOOP.md).
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
  worker agents in isolated worktrees, and opens a PR. Tasks are gated on
  executable acceptance checks (including HTTP responses validated against a
  JSON schema), and every PR gets a security pass (secrets, gitleaks, lockfile
  license policy, cargo audit) whose summary is posted as a PR comment. Hard
  escalation triggers (fewer passing tests than the base commit, 80%/100% of
  the budget, three failures in a row, a force-push outside `wingman/auto/*`)
  fire while the run is live and block auto-merge. The concurrency cap narrows
  under provider rate limits and host CPU load, a task about to become ready
  gets its worktree created and built ahead of assignment, and on autopilot a
  worker whose turn gate keeps failing is rolled back to its last green state.
  A merge conflict the one-shot resolver cannot clear goes to merge-fixer
  workers before the run stops. After each merged run the project knowledge
  layer (architecture summary, decisions, merge hotspots) is updated, by a
  knowledge-keeper agent on autopilot, and the planner reads it back. On
  autopilot, multi-file work that never called the `checkpoint` tool is failed
  before review. A critic model, which can be required to come from a different
  model family than the workers, adds guardrail tasks to the plan and can veto
  auto-merge. Cross-run stats count a task as a first-try success only when no
  retry rung ran. The daemon polls opened PRs for their post-merge outcome on
  its own cadence, and `wingman pilot eval` scores canned goals, with an LLM
  judge grading each run's diff against a golden commit, and fails on a
  regression against a committed baseline (run weekly by
  `.github/workflows/eval.yml`). See [PILOT-MODE.md](PILOT-MODE.md).
- **`wingman knows`.** Prints what Wingman knows about the current project:
  memories, skills, model routing, the verification gate, the metrics
  summary below, and index freshness. It flags stale memories: ones naming a
  project file that is gone, or a code symbol no source file defines or
  mentions and no language server's `workspace/symbol` knows.
- **`wingman metrics`.** The numbers that say whether any of this is
  working, for the current repo: time to first token (median and p90 of
  each session's first turn), tokens per completed task, verified-done rate
  (of gated turns, and of sessions), and routing pass-rates by task class and
  model. Read from the session transcripts, where the agent loop records each
  turn's `first_output_ms` and last verification receipt on its `stop`
  record — so every surface (TUI, `--print`, pilot workers, `serve`) counts.
  `--json`; also `GET /v1/projects/{p}/metrics` and the panel's Insights view.
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
- **Claude Code hook import.** `[hooks].import_claude_code = true` runs the
  hooks from an existing Claude Code `settings.json`, so arriving from Claude
  Code doesn't mean rewriting a working hooks block. Matchers are translated
  (`Bash` → `run_shell`, `Edit` → `edit_file`, …) rather than copied, since a
  verbatim matcher would import cleanly and then never fire; anything
  untranslatable is reported. Off by default — hooks run shell commands — and
  a project-level `.claude/settings.json` needs `wingman trust` just as
  `.wingman/config.toml` does. `wingman doctor` says when an importable file
  is present.
- **Background shell jobs.** `run_shell` blocks the turn and is capped at
  600s, which rules out dev servers, watch processes, and cold builds of a
  large workspace. `background: true` starts the command and returns a job id
  instead; `job_output`, `job_send`, `job_stop`, and `job_list` control it — `job_send`
  writes to the job's stdin, so a REPL or an interactive prompt can be driven
  across tool calls instead of one-shot. (A pipe, not a pseudo-terminal: no
  colours, no pagers, no full-screen TUIs — all noise for an agent.) Output is
  buffered to 128 KiB keeping the most recent bytes (and says when it dropped
  earlier ones). A background command goes through the same permission gate,
  denylist, sandbox, and credential scrub as a foreground one, and every job is
  killed with its whole process tree when the session ends — a forgotten dev
  server doesn't outlive the agent.
- **Debugger tools (DAP).** Where the LSP tools ask the compiler, `debug_start`,
  `debug_breakpoints`, `debug_continue`, `debug_state`, `debug_eval`, and
  `debug_stop` ask the runtime: launch a program or test under whatever Debug
  Adapter Protocol adapter is on `PATH` (`lldb-dap`/`codelldb` for Rust/C/C++,
  debugpy for Python, `dlv dap` for Go), stop at a line, read the stack and the
  top frame's locals, evaluate an expression, step. They need the same grant as
  `run_shell`, start the adapter through its preparation (denylist, sandbox,
  credential scrub), and kill the adapter and debuggee as one process tree on
  stop or session end. No adapter installed is a note naming what to install;
  `wingman doctor` lists what it found. Not validated live: tested against an
  in-process fake adapter only — no real lldb-dap, debugpy, Delve, or CodeLLDB
  was run. See [TOOLS.md](TOOLS.md#debugger--ask-the-runtime).
- **Two-layer loop protection.** The tools layer nudges the model when it
  repeats a call with identical arguments (`[tools].repeat_thresholds`,
  advisory, never blocks). Above it, a rolling window
  (`[tools].loop_window` / `loop_warn_at` / `loop_abort_at`) counts
  *occurrences* rather than consecutive runs — so it also sees an alternating
  `grep X → read_file Y → grep X` cycle, which resets the consecutive chain and
  is invisible to it — and ends the turn with stop reason `loop_detected`
  rather than only advising. Nudging is right for an interactive session where
  someone can hit Esc; it is not a control for `wingman pilot`, a subagent, or
  a `--print` run in CI. Set `loop_abort_at = 0` to disable.
- **Deferred tool schemas.** `[tools].defer = ["mcp__*"]` withholds matching
  tools' schemas from every request; the model finds them with `tool_search`
  (keyword → name, description and schema) and invokes them with `tool_call`.
  Every tool's schema is billed on every turn, and an MCP server's twenty are
  wanted on perhaps one — this trades a round trip in that turn for the
  schemas in the other forty-nine. Deferring is not disabling: `tool_call`
  dispatches through the same permission gate, hooks, audit trail and
  redaction as a direct call. `wingman context` reports the real total,
  including the two meta-tools it adds.
- **Steering a running turn.** Type while the agent is working and press
  Enter: the message is folded into the turn at the next provider round-trip
  instead of starting a new one. Previously the only way to redirect was
  Ctrl+C and retype, which threw away everything the turn had established. The
  model is told the message arrived mid-work, so "actually, keep the patch
  small" reads as an adjustment rather than a new task.
- **`wingman doctor --fix`.** Every config struct uses `deny_unknown_fields`,
  so one stale or mistyped key fails the whole load with `unknown field
  \`loop_abort\`` — naming no file, no line and no correction. `doctor` now
  reports which file and line, and `--fix` renames unambiguous misspellings
  after copying the file to `config.toml.bak-<timestamp>`. Only unambiguous
  renames: a key nothing matches, or one equidistant from two candidates, is
  reported and left alone. `--lint --json` is the read-only CI preflight.
- **PDF reads.** `read_file` on a `.pdf` extracts its text, the same way it
  already renders `.ipynb` cells instead of raw JSON — a spec handed over as a
  PDF is ordinary coding context. A scan with no text layer says it needs OCR
  rather than returning an empty string. Behind the default-on `pdf` feature.
- **Device pairing for `wingman serve`.** `--pair` prints a single-use,
  10-minute code that another device exchanges once for the API token, instead
  of hand-carrying a 43-character secret to it. Enrolment only — the paired
  device gets the same token and the same ceiling. See
  [HTTP-API.md](HTTP-API.md#pairing-a-device).
- **Web tools.** Built-in `web_fetch` (URL → text) and `web_search`
  (DuckDuckGo HTML, no API key) tools pair for "look something up".
- **Atomic multi-file patches.** The `apply_patch` tool applies a
  multi-file edit block atomically — no partial writes on failure.
- **Working-tree checkpoints.** `wingman checkpoint` snapshots the tree
  into a tagged `git stash`; `wingman undo` restores the most recent one.
- **Rewind timeline.** Every file edit the agent makes is already an undo
  checkpoint (`/undo [n]`); the TUI and `--print` now tag each with the session
  and turn that made it. `/rewind` in the TUI lists them one point per turn,
  with the files touched, and Enter previews what restoring to before a point
  would change, diff and all; `y` confirms. The restore is itself a checkpoint
  — none is ever deleted — so it shows at the top of the timeline and is
  undone the same way. `t` additionally truncates the conversation to before
  that turn, by forking the transcript and continuing in the fork. The panel's
  conversation view has the same timeline, preview and confirmation, over
  `GET/POST /v1/projects/{p}/sessions/{id}/rewind[/{seq}]`.
- **`wingman init`.** Scans the project (Cargo.toml, package.json,
  pyproject.toml, go.mod, …) and writes a starter `WINGMAN.md`.
- **`wingman cost`.** Per-model token + USD spend table derived from
  `~/.wingman/usage.json` and `pricing.rs`.
- **`wingman session list / fork`.** Browse recent session JSONLs;
  fork an old session (optionally truncating to N records) and resume it.
- **Session export.** `wingman session export <id> --format md|html|json`
  reduces a transcript to what a reviewer asks about: the task and the last
  answer, files changed with lines added and removed (counted from the
  successful `edit_file`/`edit_symbol` diffs, `apply_patch` patches and
  `write_file` contents in the log; `lsp_rename` and `lsp_code_action` list
  their files without line counts), every verification receipt, cost and
  tokens, and the tool-call timeline. Everything taken from the model, a
  tool or the user goes through the same secret redactor as tool output
  first, and the report says how many it caught. The TUI's `/export [md|html|json]`
  writes the current session's report to `.wingman/exports/`,
  `GET /v1/projects/{p}/sessions/{id}/export` serves it, and the panel's
  conversation view copies it or downloads it. `wingman pilot export` does the
  same for a pilot run, as a PR description with a row per worker session.
- **User-defined slash commands.** Drop a markdown file at
  `~/.wingman/commands/<name>.md` (or `<project>/.wingman/commands/`) and
  it becomes `/<name>` in the TUI. `$ARGS` is substituted.
- **In-transcript search.** `/find <query>`, `/findnext`, `/findprev`,
  `/findclear` walk hits inside the current transcript. Mouse wheel
  scrolling is enabled.
- **File-tree sidebar.** `Ctrl+B` toggles a left-side file browser; `j`/`k`
  move, `Tab` descends, `Enter` inserts the path into the composer, `v` opens
  the file in a read-only, line-numbered, syntax-highlighted view.
- **Syntax highlighting.** Fenced code blocks in the transcript and the file
  view are highlighted with tree-sitter for every parsed language. `diff`
  fences stay plain (decisions/0016).
- **`@file` attachments.** Write `@src/main.rs` in the composer and the file's
  contents are inlined into the prompt, saving the agent a `read_file` round
  trip. Image files (`png`/`jpg`/`gif`/`webp`) are base64-encoded for
  vision-capable providers instead. An unresolvable token is left as literal
  text with a warning, never silently dropped.
  Bounded so that the feature meant to save context cannot exhaust it:
  64 KiB per file, 256 KiB across one prompt, and 3 MiB for an image (which is
  refused rather than truncated, since a partial image is not a smaller image).
  A truncated attachment says so and names the path, so the agent can read the
  rest with `read_file`'s `offset`/`limit`.
- **Themes.** `tui.theme = "default" | "light" | "mono"`, plus optional
  per-role color overrides under `tui.colors` (`"#rrggbb"` hex or named).
  Highlighted code follows the theme; `mono` marks scopes by weight and slant
  instead of hue. A non-empty `NO_COLOR` environment variable wins over both
  and draws the whole TUI without colour.
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
  one-shot review prompt against the diff. `--comment` posts the findings
  back as one GitHub review with inline comments anchored to their diff
  lines (via `gh api`), skipping any a previous run already posted;
  `--dry-run` prints the payload instead of posting.
- **Local model auto-discovery.** `wingman discover` probes localhost
  Ollama / LM Studio / vLLM and prints available models.
- **Skill auto-extraction.** `wingman skill extract` scans recent session
  JSONLs for repeated tool-call sequences (e.g. `grep → read_file →
  edit_file`) and writes draft skill markdown files under
  `~/.wingman/skills/proposed/` for you to review.
- **Tree-sitter powered code understanding.** Deep language-aware parsing
  (Rust, Python, JavaScript, TypeScript, Go, C++, Java, Kotlin) for semantic chunking in the RAG
  index (re-chunked incrementally from a cached tree when a file changes),
  symbol extraction, AST-aware diffs, and outline generation. Feature-gated
  so the workspace builds without the C toolchain if you don't need parsing.
- **LSP-backed code intelligence.** Real, *resolved* go-to-definition,
  find-references, hover, diagnostics, and project-wide rename via whatever
  language server you have on `PATH` (rust-analyzer, pyright/pylsp,
  typescript-language-server, gopls) — the semantic upgrade over the
  tree-sitter heuristics. Tools `lsp_definition`, `lsp_references`, `lsp_hover`,
  `lsp_diagnostics`, `lsp_rename` degrade gracefully to the heuristic tools when
  no server is installed. `who_calls` answers from the server's call hierarchy
  (then its references) when one is installed, and says which method it used.
  See [LSP.md](LSP.md).
- **LSP-backed verification receipts.** The post-edit turn gate can fold the
  language server's diagnostics for the *changed* files into the verdict
  (`[verify].lsp_diagnostics`), so a change that introduces a type error the
  compile step missed fails verification: `✓ builds  ✓ affected tests  ✓ 0 new
  LSP diagnostics`. The affected-tests stage runs only the tests that reference
  the symbols edited this turn, found through the server's references (else a
  tree-sitter name match in test code), and falls back to the changed crates
  when a change can't be tied to a symbol; the receipt says which.
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
  model. Caveat worth knowing: compaction, titles and commit messages are
  computed without a model call at all, so only the calls that do use a model
  follow it — subagents by their `task_class`, and `wingman distill` and
  `wingman explain` as `summarize`. Everything else stays on the session
  model. For a real guarantee use `[privacy].local_only` and `wingman attest`.
- **Learned routing.** Every verification-gate result — from `--print`, the
  TUI and pilot workers — is recorded in `~/.wingman/learn.db` against the
  task class (`default` for a session, the role for a pilot worker) and the
  `provider/model` that ran it. `wingman router backfill` adds a later,
  durable verdict for merged pilot PRs (`held` / `reverted` / `unknown`,
  where an untouched PR is `unknown`, never a pass), and `wingman router
  stats` shows both per class. Setting `[router].learned_min_samples` turns
  on learned routing: once a model has that many gate results for a class in
  this repo, the best of them (skipping any whose PRs were reverted more often
  than they held) serves that class — the session model when no `--model` is
  given, and each pilot worker role's first attempt. Off by default; it only
  chooses among models that have already run the class and whose provider is
  still configured (and local, under `[privacy].local_only`).
- **Explain-and-teach.** `wingman explain` gives a per-file "what changed and
  why it matters" walkthrough of the working diff (routed as the `summarize`
  class, which is the fast model unless `[router.classes]` says otherwise),
  for reviewers and juniors.
- **Audit trail.** `[audit].enabled = true` appends a JSONL record (timestamp,
  tool, redacted input, error flag) for every tool call — a compliance trail
  for teams.
- **Benchmark harness.** `wingman bench` runs a suite of prompts and records
  time to first token, tokens per completed task, verified-done rate, and
  routing outcomes per served model — the same definitions as
  `wingman metrics`. `--json` or `--markdown` prints a publishable report.
- **Embeddable.** Use `wingman-core` as a library or drive Wingman from any
  language over MCP (`wingman mcp-serve`). See [SDK.md](SDK.md).
- **Visual verification.** *(Opt-in build.)* Build with `--features browser`
  and set `[verify.browser].url` to make the turn gate load a URL, screenshot
  it, and fail if it drifts from a baseline. Not in the default build, and it
  fails open — with no browser present the gate passes rather than blocking.
- **Agent browser.** *(Opt-in build.)* The same `--features browser` build
  gives the agent a `browser` tool: one headless Chrome tab, started on first
  use and closed with the session, that it can navigate, click and type into,
  screenshot (saved under `.wingman/browser/` — tool results can't carry
  images, so the model gets a path), read console errors from, and run JS in.
  Localhost dev servers are the target: under `[privacy].local_only` it opens
  loopback URLs only. `wingman doctor` says whether a Chrome binary was found.
  Unit-tested and compile-checked, but not yet run end to end against a real
  Chrome. See [TOOLS.md](TOOLS.md#browser).
- **Server-backed team memory.** Beyond the git-backed `memory sync`,
  `wingman memory push` / `pull` sync memories through a team HTTP endpoint
  (`[team]`), merging non-destructively.
- **Multi-channel pilot intake.** `wingman pilot intake slack | email`
  turns Slack events or delivered `.eml` files into pilot requests.
- **Sandboxed pilot workers.** A task whose plan touches dependencies, build
  scripts or risky commands runs its worker in `docker run` against a copy of
  its worktree; migrations, infra and irreversible work run in a Firecracker
  microVM (Linux + KVM, optionally jailed). The resulting diff is applied back
  and committed; CPU, memory, pid and network limits come from
  `[pilot.sandbox]`. Without Firecracker the vm tier stays fail-closed, and
  `wingman doctor` reports which tiers are available. Unvalidated against a
  real Docker daemon or Firecracker host. See
  [PILOT-MODE.md](PILOT-MODE.md#sandbox-tiers).
- **Tool synthesis.** A pilot worker that keeps needing a command the toolset
  lacks calls `propose_tool`; the proposal lands in `.wingman/tools/` as a
  custom command tool and every registry built after approval carries it, so
  the next worker can call it by name. Approval is automatic only on
  `autopilot` in a trusted project, otherwise `wingman pilot tools approve`.
  Synthesized tools run under `run_shell`'s own guards. Unvalidated against a
  live provider. See [PILOT-MODE.md](PILOT-MODE.md#tool-synthesis).
- **Watch mode.** `wingman pilot daemon --watch` wakes the discovery daemon
  between polls. A saved file runs the local sources, including `// ASK:`
  comments. A commit, merge, checkout or rebase runs every source, through
  hooks `wingman pilot hooks install` writes. The hooks run the wingman binary
  directly on every platform, no shell script. Candidates go through the same
  queue, trust and per-cycle dispatch cap. See
  [PILOT-MODE.md](PILOT-MODE.md#watch-mode).
- **Provider validation matrix.** `wingman pilot validate-providers` runs one
  canned pilot plan (add a `--version-only` flag to a throwaway CLI) against
  every configured provider that has credentials, each in a scratch repo under
  a strict USD and token cap, and writes a pass/fail/skipped matrix to
  `.wingman/provider-validation/matrix.md` and `matrix.json`. A pass means the
  flag reached the merged integration branch, not that the worker said so.
  See [PILOT-MODE.md](PILOT-MODE.md#validating-your-providers).
- **Skill packs.** `wingman pilot skills install | search | list | verify`
  shares pilot roles as versioned packs from a git-hosted index, resolving
  dependencies with caret rules and refusing unsigned packs unless told
  otherwise; signatures are checked with `ssh-keygen`. See
  [PILOT-MODE.md](PILOT-MODE.md#skill-packs).
- **Pilot answers review on its own PRs.** With `pr_reviews` in
  `[pilot.daemon].sources`, the daemon picks up trusted reviewers' unresolved
  threads on the PRs pilot opened. It fixes them on the same branch, pushes,
  replies on each thread, and resolves only the threads the push actually
  changed. Rounds per PR are capped and share `[pilot].max_usd`. See
  [PILOT-MODE.md](PILOT-MODE.md#review-rounds-on-pilots-prs).
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
- **Warm index daemon.** `wingman indexd start` keeps `.wingman/index.db` fresh
  in the background; `stop` and `status` manage it. Liveness is a real process
  check (`kill(pid, 0)` / `OpenProcess`), so a crashed daemon's pidfile is
  cleared rather than reported as running. The TUI opens on the daemon's warm
  index instead of indexing again.
- **Import-aware prefetch.** Reading a file pre-warms the files it imports,
  then its siblings, so the agent's next read hits a warm cache.
- **Search escalation.** Before each user turn the request is run against the
  project index and the best-matching files, line ranges and symbols go into
  the turn, so the agent starts by reading the right code instead of
  grepping for it; the system prompt and `grep`'s description steer
  concept-level lookups to `semantic_search` first. The block is capped at
  `[learn].search_hint_tokens` (default 300; `0` turns it off).
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
