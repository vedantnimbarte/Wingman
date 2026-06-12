# Differentiation Roadmap

Goal: make Arc-Code measurably **faster**, produce **more trustworthy results**, and give users
reasons to **prefer it daily** over Claude Code, Codex CLI, Cursor CLI, and similar agents.

This document complements `plan.md` (Pilot/autonomous mode). It covers the single-agent
everyday experience, where most usage and retention is won or lost.

The strategy in one sentence:

> **Arc-Code is the coding agent that already knows your repo, picks the right model for every
> step, proves its work, and gets smarter every session.**

Each clause maps to a feature pillar below. Competitors have none of the four end-to-end;
Arc-Code already has partial infrastructure for all of them.

---

## Competitive gaps we attack

| Weakness in competitors | Arc-Code asset that exploits it |
|---|---|
| Single-provider lock-in (Claude Code → Anthropic, Codex → OpenAI) | `Provider` trait + 9 adapters + 70-odd OpenAI-compatible endpoints (`arccode-providers`) |
| No memory between sessions; repo knowledge rebuilt every run | Learning loop: memories, skill stats, session embeddings (`arccode-learn`) |
| Search = grep fan-out; slow and token-hungry on large repos | RAG index + tree-sitter semantic chunks (`arccode-rag`, `arccode-ts`) |
| "Done" without proof; confidently broken output | Turn gate, checkpoint verify, acceptance checks (`arccode-autonomous`) |
| Unix-first, janky on Windows | Developed and tested on Windows from day one |

---

## Pillar 1 — Model routing ("right model for every step")

**Status today:** `RouterConfig` exists with a fallback chain and a `router.fast_model` knob,
but the session runs one model for everything. No competitor routes per task class.

**Build:**

1. **Task-class router** (`arccode-core`): classify each model call as one of
   `{reason, codegen, search_summarize, commit_message, title, compaction}` and resolve the
   model per class from `[router]` config:
   ```toml
   [router]
   default      = "anthropic/claude-opus-4-8"
   fast_model   = "anthropic/claude-haiku-4-5-20251001"
   classes      = { compaction = "fast", search_summarize = "fast", commit_message = "fast" }
   ```
   Compaction, tool-output summarization, and title generation alone are ~20–40% of calls and
   need no intelligence — routing them to a fast/cheap model is a pure latency + cost win.
2. **Learned routing** (`arccode-learn`): the skill-outcome scoring in `learn.db` already
   records success/failure per task. Extend the schema with `(task_class, model, outcome)` and
   surface `arccode router stats` showing which model wins per class *in this repo*. Opt-in
   auto-routing once enough samples exist.
3. **Local-model preset:** a curated `[router]` profile that keeps `search_summarize`,
   `commit_message`, and `compaction` on a local Ollama model (`arccode discover` already finds
   them). Privacy story: "simple steps never leave your machine."

**Integration points:** model resolution in `arccode-core` (config defaults → CLI flags →
per-provider); `LearningHook::before_turn()` for learned overrides.

**Why it differentiates:** requires a provider-agnostic core, which Claude Code and Codex
structurally don't have. It is mostly plumbing on top of what exists.

---

## Pillar 2 — Warm repo: persistent index daemon ("already knows your repo")

**Status today:** `arccode-rag` builds a per-project SQLite index (`.arccode/index.db`) with
embedded semantic chunks from `arccode-ts`; a `notify`-based file watcher (500 ms debounce)
exists; `semantic_search` is a registered tool. What's missing is *always-on freshness* and
*default-on usage*.

**Build:**

1. **`arccode indexd`** — a detached background process per project that keeps `index.db`
   fresh via the existing watcher. TUI startup checks for a live daemon; if present, the session
   opens with a warm index (target: <100 ms to first prompt).
2. **Search escalation policy:** teach the agent loop to try `semantic_search` *before* grep
   for concept-level queries, and inject top symbols/files for the user's prompt into context
   via `LearningHook::before_turn()`. Fewer search round-trips = fewer tokens = faster + better.
3. **Symbol graph queries:** expose `who_calls(symbol)` / `defined_in(symbol)` as a tool backed
   by tree-sitter symbol extraction. Answers in milliseconds what competitors burn 3–5 model
   turns discovering.
4. **Benchmark + publicize "time to first useful token"** vs Claude Code and Codex on a large
   repo (e.g., 100k+ LOC). Speed users can *feel* in the first 10 seconds drives adoption.

**Integration points:** `arccode-rag` (index, watcher), `arccode-ts` (symbols),
`crates/arccode-tools/src/registry.rs` (new tools), TUI startup path.

---

## Pillar 3 — Verification receipts ("proves its work")

**Status today:** `arccode-autonomous` has `checkpoint::verify()`, executable acceptance checks
(shell/grep/HTTP), and a `turn_gate_cmd` (default `cargo check --workspace`) — but only in
Pilot mode. Single-agent sessions can still claim "done" unverified.

**Build:**

1. **Pull the turn gate into the default agent loop:** after any turn that edited files, run a
   project-appropriate check command (auto-detected: `cargo check` / `tsc --noEmit` /
   `python -m compileall` / configured override) before the stop is accepted. Failure feeds the
   error back as a tool result so the agent self-corrects.
2. **Affected-test discovery:** use the symbol graph (Pillar 2) to map edited symbols → test
   files that reference them, and run just those. Full-suite runs stay opt-in.
3. **Verification receipt in the TUI/headless output:** a compact block on completion —
   `✓ builds  ✓ 3/3 affected tests  ✓ no new lint errors` — with each check expandable.
   Receipts are also written to the session JSONL so `arccode session list` shows which past
   runs were verified.
4. **Config:** `[verify] turn_gate = "auto" | "<cmd>" | "off"`, `affected_tests = true`.

**Integration points:** stop/turn-end logic in `arccode-core`,
`LearningHook::after_turn()/after_stop()`, reuse of acceptance-check executors from
`arccode-autonomous` (consider lifting them into `arccode-core` or a shared crate).

**Why it differentiates:** the #1 complaint about every coding agent is confidently broken
code. "Never says done without proof" is a trust position competitors haven't taken by default.

---

## Pillar 4 — Compounding team memory ("smarter every session")

**Status today:** 4 memory types as markdown+frontmatter in `~/.arccode/memory/` (global) and
`<project>/.arccode/memory/` (project); skill stats in `learn.db`; session embeddings in
`sessions.db`. This is already ahead of every competitor — the gaps are distillation,
sharing, and visibility.

**Build:**

1. **Post-session distillation:** an `after_stop()` hook pass (routed to the fast model —
   Pillar 1) that extracts durable repo facts from the session: build quirks, conventions, "the
   tests need X env var", gotchas. Write them as project memories automatically (with a
   review queue in the TUI rather than silent writes).
2. **Team-shared memory:** document and support committing `.arccode/memory/` (and optionally a
   sanitized skills subset) to git. Add `arccode memory sync` to merge teammate memories without
   clobbering local ones. A new teammate's agent starts with the team's accumulated knowledge —
   the switching-cost moat.
3. **`arccode knows`:** a command + TUI panel rendering "what I know about this project"
   (memories, learned router stats, top skills, index freshness). Making the invisible asset
   visible is what makes users *value* it — and reluctant to switch away from it.
4. **Memory hygiene:** staleness scoring (memories referencing files/symbols that no longer
   exist get flagged via the index), so quality compounds instead of rotting.

**Integration points:** `arccode-learn` (memory store, hooks), `arccode-skills`,
`arccode-rag` (staleness checks), TUI panel, CLI subcommand in `arccode-cli`.

---

## Supporting bets (cheaper, scheduled opportunistically)

- **Speculative parallelism** (`arccode-core`): while the model streams, pre-read files it is
  likely to touch next (from index proximity to already-read files) and pre-warm `git status`/
  build caches. Rust + tokio make this near-free; perceived latency drops.
- **Fearless rewind:** auto-snapshot the worktree before each mutating tool call (shadow
  `git stash create` or worktree clone) with a TUI timeline to scrub back. Users grant more
  autonomy when undo is one keystroke — directly increases usage depth.
- **Explain-and-teach mode:** optional per-hunk "why" annotations attached to diffs (fast-model
  generated), browsable in the TUI diff view. Loved by juniors and reviewers; no CLI agent has it.
- **Windows as a first-class story:** keep CI green on Windows, ship winget/MSI packaging, test
  PowerShell quoting paths in `shell` tool. An underserved market the Unix-first competitors
  concede by neglect.
- **Repo health autopilot:** once Pilot M2/M3 land, a low-priority scheduled run (existing
  `[schedule]` config) that uses idle time + cheap models to clear lint debt and propose small
  PRs. Converts Arc-Code from "tool I invoke" to "teammate that's always producing value."

---

## Sequencing

| Phase | Deliverables | Status |
|---|---|---|
| **D1** | Task-class router (static config) + fast-model defaults for compaction/summaries/titles | ✅ **Shipped** (2026-06-12): `[router.classes]` config + `RouterConfig::resolve_class`; `spawn_subagent` gained `task_class` and routes through it. Remaining: model-based compaction/title call sites (none exist yet — recap is heuristic) |
| **D2** | `arccode indexd` + search escalation + symbol-graph tool | Not started |
| **D3** | Default-loop turn gate + affected tests + verification receipts | ✅ **Core shipped** (2026-06-12): `TurnGate` trait + gate retry loop in `AgentLoop`, `AgentEvent::Verification` receipts in TUI/headless, `[verify]` config (`turn_gate = "auto"/"off"/cmd`), auto-detection per ecosystem (`ShellTurnGate` in `arccode-cli/src/runtime.rs`). Remaining: affected-test discovery (needs D2 symbol graph) |
| **D4** | Post-session distillation + `arccode knows` + team memory sync | 🔶 **Partial**: `arccode knows` shipped (2026-06-12); `arccode memory export/import/diff` already existed and covers pack sharing. Remaining: post-session distillation, staleness scoring |
| **D5** | Learned routing, speculative parallelism, rewind timeline | Not started |

Each phase is independently shippable and demo-able. D1 and D2 can proceed in parallel
(different crates, no shared files).

---

## Metrics to track from day one

- **Time to first useful token** (session start → first model output with context loaded).
- **Tokens per completed task** (routing + index should drive this down ~30–50%).
- **Verified-done rate** (% of sessions ending with a green receipt).
- **Return rate** (sessions per user per week) — the actual goal behind all of the above.

---

*Created 2026-06-12. Companion to `plan.md` (Pilot mode) — this doc owns the single-agent
experience pillars; Pilot owns multi-agent orchestration.*
