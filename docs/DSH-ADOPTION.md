# What to Take From DeepSeek Harness

A review of [`deepseek-ai/deepseek-harness`](https://github.com/deepseek-ai/deepseek-harness)
(`dsh`) against Wingman, and a proposal for what is worth adopting.

DSH is a TypeScript agent harness built on [Cordis](https://github.com/cordiverse/cordis),
a plugin framework. Its organizing claim is that **everything is a plugin** —
the model adapter, the tool registry, the session log, and the agent loop
itself are all config rows in a plugin tree, replaceable without patching a
core. It ships ~50 package families and a generated documentation system to
keep that tree comprehensible.

Wingman and DSH overlap heavily: both have MCP (host and server), skills,
subagents, ACP, sandboxing, hooks, plan mode, compaction, session
fork/resume, and multi-provider support. In several areas Wingman is ahead —
provider breadth, LSP depth, the verification gate, cost arbitrage, and
Windows support. This document only covers the delta in DSH's favour.

Effort estimates are rough and use this scale:

| Size | Meaning |
|---|---|
| **S** | Under a day. One or two files. |
| **M** | Two to five days. A new module, no cross-cutting change. |
| **L** | One to three weeks. Touches multiple crates or a public contract. |
| **XL** | Months. A structural change to how Wingman is built. |

---

## Summary

| # | Item | Size | Verdict |
|---|---|---|---|
| A1 | Event-sourced session log | **L** | Do it, but let P1/P4 force the timing. Diagnostic gap, not corruption. |
| A2 | Widen existing traits into named seams | **M**, incremental | Do it opportunistically. |
| A3 | Full plugin runtime (Cordis-equivalent) | **XL** | **Don't.** Reasoning in §A3. |
| P1 | Tool-output spill | S–M | **Done** — `[tools].spill_tool_output`. |
| P2 | Repeat-tool-call guard | S | **Done** — `[tools].repeat_thresholds`. |
| P3 | Background jobs | M | Do it. |
| P4 | Tool-result pruning before compaction | S–M | **Done** — `[tools].prune_threshold_chars`. |
| P5 | Per-call tool deadlines | S | **Done** — `[tools].tool_timeout_secs`. |
| P6 | Named tool presets | S | **Done** — `--preset review`, 24 tools → 13. |
| P7 | `@file` references in the composer | — | **Already existed.** See note below. |
| P8 | Session search over SQLite FTS | S | **Done** — one line; reused `search_hybrid`. |
| P9 | Hook bridges (Claude Code / Codex) | M | Adoption play, not a capability. |
| P10 | Per-message feedback | M | Replaces an admitted heuristic. |
| P11 | Persistent PTY sessions | L | After P3. |
| P12 | Code Mode (`run_code` + generated SDK) | L–XL | Prototype behind a flag. Don't lead with it. |
| E1 | Decision records | S to start | Do it. |
| E2 | Generated documentation | M | Do it for the drift-prone docs only. |
| E3 | Runtime invariants | M | Do it alongside A1. |

---

# Part 1 — Architecture

## A1. The session log should be the source of truth

**This is the most valuable idea in DSH's codebase, and it does not require
adopting Cordis.**

DSH holds one invariant: *model-visible means logged.* Everything the model
sees must be reconstructable from the append-only session log, and model
history is **derived** from that log by a projection function
(`deriveMessages()`) rather than maintained beside it. A runtime assertion
enforces it. The consequence is that adding a new kind of model-visible input
*requires* extending the session event type — there is no way to slip
something into the model's context that replay would not reproduce.

Wingman is currently the other shape. [`AgentLoop`](../crates/wingman-core/src/agent.rs:273)
owns `history: Vec<Message>` as the live source of truth, and the JSONL log is
written separately, as a side effect, by three different callers:

- `crates/wingman-tui/src/app.rs`
- `crates/wingman-cli/src/commands/headless.rs`
- `crates/wingman-cli/src/serve/sessions.rs`

[`SessionRecord`](../crates/wingman-session/src/lib.rs:53) is a closed enum of
six variants — `SessionStart`, `User`, `Assistant`, `ToolResult`,
`UsageDelta`, `Stop`. Several things the model demonstrably sees have no
variant at all. The compaction recap built in
[`tokens.rs`](../crates/wingman-core/src/tokens.rs:112) is spliced directly
into `history` and never logged. Learning-hook injections
(`LearningHook::before_turn`) return text that is prepended to a turn and
never logged. Tool output is logged in full but the model sees the
[truncated form](../crates/wingman-core/src/agent.rs:651).

Two representations, three writers, no invariant tying them together.

The divergence is deliberate, and the codebase already documents it. From
`crates/wingman-tui/src/app.rs:1462`:

> We record the conversation turn-locally (user prompt + assistant reply)
> rather than snapshotting `agent.history()`, because compaction rewrites
> history mid-conversation and would invalidate any index into it.

That is a workaround for precisely the problem A1 solves: the log cannot be
derived from history because history is unstable, so the two are written
independently and are structurally guaranteed to drift.

Two further specifics found while verifying this:

- The compaction pass is `history.splice(0..replaced, once(recap))` in
  [`agent.rs`](../crates/wingman-core/src/agent.rs:418). Nothing appends a
  record. History and log diverge in *both* directions after a compaction:
  the log holds messages the model can no longer see, and the model sees a
  recap the log has never heard of.
- `SessionRecord::SessionStart.system_hash` is **always `None`** — every
  construction site passes it ([`lib.rs:378`](../crates/wingman-session/src/lib.rs:378),
  [`headless.rs:79`](../crates/wingman-cli/src/commands/headless.rs:79)). The
  log records nothing about the system prompt at all, so the learning hook's
  per-turn system injection is invisible twice over.

### What this does and does not break

Worth stating precisely, because the scope is narrower than it first looks.

**Not broken.** `session replay` re-runs prompts fresh, and `session fork`
rebuilds from the full log — which still holds the original messages a
compaction folded away. A fork taken after a compaction gets the
*un-compacted* history, which is arguably better than what the parent had.
These work.

**Broken.** You cannot reconstruct what the model actually saw at a given
turn. That is a diagnostic and audit failure, not a data-corruption one:

- The audit trail (`[audit].enabled`) records every tool call but cannot
  answer "what was in the model's context when it decided that" — which is
  the question an audit trail exists to answer.
- Debugging a bad turn means guessing at the recap and the injected system
  text, because neither was written down.

This is why A1 is sequenced *with the features that need it* rather than as
urgent remediation — see [Suggested sequencing](#suggested-sequencing).

It also matters for everything in Part 2. Spill (P1) adds a locator the model
sees. The repeat guard (P2) injects an advisory the model sees. Pruning (P4)
rewrites a result the model sees. Each of those, under the current design,
requires deciding *separately* how it interacts with persistence, and each is
a chance to get it wrong the same way compaction already has. Under the
invariant, each is one new event variant and the projection handles the rest.

### What the change looks like

1. Widen `SessionRecord` into a real event enum, with variants for the
   currently-invisible model-visible inputs: `Recap`, `InjectedContext`,
   `ToolResultReplaced`, `Spilled`.
2. Write a `derive_messages(&[SessionRecord]) -> Vec<Message>` projection in
   `wingman-session` or `wingman-core`.
3. Make the agent loop append to the log and derive history from it, instead
   of mutating `history` directly. Keep a cached projection for performance —
   the point is that the cache is *derived*, not authoritative.
4. Move log-writing out of the three surfaces and into the loop, so the
   surfaces render from events rather than each writing their own.
5. Add a debug-build assertion that the request Wingman is about to send
   equals the projection of the log. This is E3 (§E3) and it is what keeps
   the invariant true a year from now.

Steps 1–2 are additive and can land first. Step 3 is the breaking one.

**Size: L.** Touches `wingman-session`, `wingman-core`, and all three
surfaces. Existing session JSONLs need either a version tag and a
compatibility path in the reader, or an accepted one-time break — worth
deciding explicitly before starting.

## A2. Widen the traits you already have

Wingman already has the seams that matter. Compare against DSH's service keys:

| Capability | DSH service | Wingman today |
|---|---|---|
| Model adapter | `ctx.llm` | [`Provider`](../crates/wingman-core/src/provider.rs:141) trait |
| Tool registry | `ctx.tools` | [`Tool`](../crates/wingman-tools/src/lib.rs:104) + [`ToolDispatcher`](../crates/wingman-core/src/agent.rs:25) |
| Turn interception | `agent/*` events | [`LearningHook`](../crates/wingman-core/src/agent.rs:39), [`TurnGate`](../crates/wingman-core/src/agent.rs:74) |
| Embeddings | — | [`Embedder`](../crates/wingman-rag/src/embedder.rs:15) |
| MCP | `ctx.mcp` | [`McpClient`](../crates/wingman-mcp/src/lib.rs:48) |
| Session store | `ctx.sessions` | **concrete `SessionLog`** |
| Filesystem | `ctx.fs` | **concrete** |
| Subprocess / shell | `ctx.shell`, `ctx.subprocess` | **concrete in `run_shell`** |
| Storage | `ctx.storage` | **concrete, scattered** |

The gap is the bottom four, and one of them earns a trait for a real reason
rather than on principle. DSH's filesystem and subprocess providers share one
execution world, so pointing both at a remote sandbox (their `e2b` package)
moves Bash, PTY, *and* LSP with them — no per-tool forks. That is a genuine
architectural payoff, and it is the shape Wingman would need if remote or
containerised execution ever becomes a goal.

Recommendation: don't do a trait sweep. Extract a seam when a second
implementation actually arrives, and make `run_shell`'s process spawning the
first one — because P3 (background jobs) and P11 (PTY) are both second
implementations of exactly that, and doing them without a shared spawn seam
means three copies of the sandbox-policy logic.

**Size: M, spread over the features that need it.**

## A3. The full plugin runtime

You asked for a serious evaluation rather than a dismissal, so here it is,
including the part that argues for it.

### What it genuinely buys

Cordis is not decoration. Four properties fall out of it that Wingman cannot
currently match:

1. **Third-party extension without a fork.** A DSH user installs
   `dsh-subagent-claude-code` from npm and gains a capability. A Wingman user
   who wants a new built-in tool sends a PR. MCP and `[[tools.custom]]` cover
   *tools*, but not a new provider, a new compaction strategy, or a new
   session backend.
2. **Reversible registration.** Everything mounts through `ctx.effect()` and
   unwinds on unload, which is what makes live reconfiguration safe. Wingman
   has one instance of this need already — `/model` swapping mid-session —
   and solves it by hand.
3. **Per-session composition.** DSH mounts a preset under a session's scope,
   so one process runs several differently-composed agents at once. Wingman's
   pilot mode wants exactly this and approximates it with separate child
   processes.
4. **Uniform interception.** One waterfall covers hooks, permissions,
   guards, spill, and pruning. Wingman has `LearningHook`, `TurnGate`, the
   registry's permission check, and the `[hooks]` shell system as four
   unrelated mechanisms — and P1, P2, P4, and P5 would each add a fifth
   through eighth.

Point 4 is the strongest argument, and it is not hypothetical: this document
proposes four features that all want the same interception point.

### What it would cost in Rust

Cordis leans on TypeScript-specific machinery that has no cheap Rust
equivalent:

- **Service-by-key lookup.** `ctx.tools` is a dynamically-typed property with
  a statically-merged type. Rust's version is a `TypeMap` of
  `Arc<dyn Any + Send + Sync>` with downcasts at every access — losing
  compile-time checking exactly where Wingman currently has it.
- **Declaration merging for events.** DSH extends `SessionEventMap` from any
  package and the compiler checks every dispatch site. Rust has no open enum.
  You get a closed enum in a core crate — which reintroduces the privileged
  core the design exists to avoid — or trait objects plus downcasting.
- **Runtime loading.** Cordis mounts plugins from `package.json` at boot.
  Rust's options are `dyn` crates over an unstable ABI, a WASM host, or
  compile-time feature flags. The first is fragile, the second is a large
  subsystem with its own capability model, the third is not a plugin system.
- **Reversible effects.** Achievable with `Drop` and a registry, but the
  ordering guarantees DSH relies on need care in a `Send + Sync` async
  context.

Then the second-order costs, which are visible in DSH's own repo: they need a
`--dump-config` command to answer "what is running", a generated config
catalog because configuration is no longer readable from source, generated
module and capability graphs because the dependency structure is no longer
apparent from the package tree, and a `cordis-primer.md` plus a tutorial
before a contributor can make a first change. That is real, recurring cost.

### The specific reason not to

Wingman's positioning is a measured number:

```
$ wingman context
  first turn         4236 tokens  before your prompt
```

A plugin tree makes that number a function of installed plugins rather than a
property of the binary. The claim becomes "4236 tokens, depending on your
configuration" — which is what every other agent already says, and is the
thing Wingman exists to be the alternative to. Beyond the token count, a
single static binary with a knowable tool set is also most of the Windows and
air-gapped story (`[privacy].local_only`, `wingman attest`); `attest` in
particular audits configured egress, and a runtime plugin loader is a new
egress channel it would have to reason about.

The honest summary: Cordis solves a real problem that Wingman does not have
yet. DSH is a platform seeking an ecosystem. Wingman is a tool with a
specific claim. If the goal becomes "third parties ship Wingman extensions",
revisit this — and revisit it as a WASM host with a capability model, not as
dynamic linking.

**Recommendation: do A1 and A2, skip A3.** They deliver points 2 and 4 above —
the two that have concrete need today — for L+M instead of XL, and neither
forecloses A3 later. An event-sourced log is in fact a *prerequisite* for a
plugin runtime, so A1 is the first step either way.

---

# Part 2 — Product features

## P1. Tool-output spill

*DSH: [`packages/spill`](https://github.com/deepseek-ai/deepseek-harness/tree/main/packages/spill)*

Wingman truncates tool output head/tail with an elision marker
([`agent.rs:651`](../crates/wingman-core/src/agent.rs:651)) and the middle is
lost permanently. DSH persists the full text to a session-scoped file and
replaces the inline result with a bounded preview **plus a retrieval
locator**, so the model can go and read the rest.

Same token cost, nothing discarded. A 4000-line `cargo test` run is currently
guillotined; spilled, the model reads the preview, sees `17 failures`, and
reads into the middle of the file for the ones that matter.

This is the best fit in the list for Wingman's stated thesis: it reduces what
the context has to hold without reducing what the agent can know.

**Implementation.** One call site changes. Write to
`.wingman/spill/<session>/<tool>-<n>.txt`, return preview + path. Because the
model can already `read_file` with an offset, no new tool is strictly needed —
though a dedicated one with a clearer contract is worth considering.

Two details from DSH worth keeping: the suggested filename is a *hint the
backend sanitizes to one path segment*, never a caller-supplied path; and
forked sessions inherit locators from the seeded log without copying or
re-owning the artifacts.

**Size: S–M.** Add retention when the directory gets big, not before — DSH
ships without a per-session cleanup policy too.

## P2. Repeat-tool-call guard

*DSH: [`guard/repeat-tool-reminder`](https://github.com/deepseek-ai/deepseek-harness/tree/main/packages/guard/repeat-tool-reminder)*

Wingman has no loop detection of any kind. DSH tracks the run length of
consecutive calls with identical canonicalized arguments, per agent, and at
configured thresholds (`[3, 5, 8]`) injects an escalating advisory telling the
model to re-read the last result and change approach or conclude. It never
blocks and never rewrites a call — the decision stays with the model, and a
legitimately repeated call is delayed by nothing.

Four details that are easy to get wrong and that they got right:

- **Canonical arguments** means deep key-sort then serialize. Otherwise
  argument ordering launders a repeat.
- **Denied calls count.** A model hammering a call the permission mode keeps
  refusing is precisely the loop worth breaking. In Wingman this means the
  guard sits *after* the registry's permission check, not before.
- **Untracked calls are transparent, not resetting.** With `update_tasks`
  excluded, `grep X → update_tasks → grep X` is still two consecutive
  `grep X`. Bookkeeping tools interleaved into a loop must not launder it.
  Wingman's exclude list wants `update_tasks` and `task_complete`.
- **Per-agent keying**, so a subagent's repetition never trips its parent's
  counter.

**Size: S.** Roughly 40 lines plus config in the tool registry. In-memory
only; a resumed session starting with a fresh chain is an accepted cost.

## P3. Background jobs

*DSH: [`packages/jobs`](https://github.com/deepseek-ai/deepseek-harness/tree/main/packages/jobs)*

`run_shell` blocks the turn and is capped at 60s by default, 600s maximum
([`run_shell.rs:97`](../crates/wingman-tools/src/builtin/run_shell.rs:97)).
That rules out dev servers, long test suites, watch processes, and cold
builds of any large workspace — Wingman cannot wait for a full `cargo build`
of its own repository.

DSH has a job registry with owner-fenced ids (`bash-3`), `start/poll/stop/
wait`, and completion notices delivered back into the conversation. Their
`JobKind` covers both `bash` and `subagent`, which means the same protocol
makes delegation non-blocking for free — worth copying, since Wingman's
depth-1 `spawn_subagent` blocks today for the same reason.

**Implementation.** A process table plus `run_shell(background: true)`
returning a job id, and `job_output` / `job_kill` / `job_list` tools. Not a
capability seam — three tools and a `HashMap`. Build it on the shared spawn
seam from §A2 so the sandbox policy is not copied.

**Size: M.**

## P4. Tool-result pruning before compaction

*DSH: [`compaction-tool-result-pruner`](https://github.com/deepseek-ai/deepseek-harness/tree/main/packages/compaction/compaction-tool-result-pruner)*

Wingman's [`Compactor`](../crates/wingman-core/src/tokens.rs:112) folds whole
message spans into a recap, discarding the assistant's reasoning along with
the bulk. DSH prunes over-budget **tool results in place** first — head 4096 /
marker / tail 1024 code points — and only compacts after.

Tool results are where the tokens are; the assistant's reasoning is where the
value is. Pruning first buys many more turns before the destructive fold, and
it is model-free, which fits Wingman's already-model-free `synthesize_recap`
exactly.

The idempotency property is worth copying deliberately: with
`head + marker + tail ≤ threshold`, a pruned result is strictly smaller than
the threshold, so a second pass emits nothing. It cannot oscillate.

Under A1 this becomes a `ToolResultReplaced` event that keeps the original in
the log — the model's context shrinks, the audit trail does not.

**Size: S–M.**

## P5. Per-call tool deadlines

*DSH: [`guard/timeout-policy`](https://github.com/deepseek-ai/deepseek-harness/tree/main/packages/guard)*

Only `run_shell` has a timeout. An LSP request against a cold rust-analyzer,
a `web_fetch` to a slow host, or a wedged MCP server hangs the turn
indefinitely with no upper bound. One deadline in the registry wrapping every
dispatch, overridable per tool, closes it.

**Size: S.**

## P6. Named tool presets

*DSH: [`packages/preset`](https://github.com/deepseek-ai/deepseek-harness/tree/main/packages/preset)*

The mechanism exists — `[tools].disabled_tools`, applied in
[`runtime.rs:623`](../crates/wingman-cli/src/runtime.rs:623) — but there are
no named bundles. `wingman --preset review` (read + LSP tools only) would make
`wingman context` print roughly 1200 tokens instead of 4236.

That turns the README's headline number from something the user reads into
something the user can act on, which is a better version of the same pitch.

**Size: S.** A config table over existing machinery.

Measured after implementing: `--preset review` takes this repo from 24 tools
/ 4324 first-turn tokens to 13 tools / 2923 — a 32% cut, not the ~70% the
first draft of this document guessed. The estimate was optimistic because the
system prompt (671 tokens) and the read/search tools that any session needs
are both irreducible. Still the largest single lever available without
touching what the agent can do.

## P7. `@file` references in the composer

*DSH: [`context/file-reference`](https://github.com/deepseek-ai/deepseek-harness/tree/main/packages/context)*

**Already implemented — this entry was a mistake.**
`crates/wingman-tui/src/attachments.rs` expands `@path` tokens at submit
time: text files are inlined as fenced blocks, images are base64-encoded into
`ExpandResult::images` for vision-capable providers, unresolvable tokens are
left literal with a warning, and the transcript shows the user's original
text while the model sees the expanded form. It is more complete than the DSH
feature that prompted the suggestion. The original survey missed it because
the module is named `attachments`, not `file_reference`.

One real gap remains, and it is *not* covered by the above: **there is no
size cap.** `@huge.log` inlines the entire file into the prompt, which is a
context-tax foot-gun inside the feature meant to reduce context. A per-file
byte cap with a truncation marker is ~5 lines in `expand()`. Not done here
because it is a different change from the one this entry proposed.

**Size: — (nothing to build); the size cap is S.**

## P8. Session search over SQLite FTS

*DSH: [`session-query-sqlite`](https://github.com/deepseek-ai/deepseek-harness/tree/main/packages/session-query)*

`recall_session` is embedding-based only. Wingman already fuses dense and
BM25 for the *code* index, on the correct argument that exact identifier and
error-string matches need keyword scoring — but sessions get semantic-only
search. Error strings, commit SHAs, and filenames are exactly the BM25 case.
This is an inconsistency with Wingman's own stated design, not a missing
feature borrowed from elsewhere.

**Size: one line**, revised down from M and then from S. No FTS5 table was
needed: `IndexStore::search_hybrid` already existed, already fused dense and
BM25 via reciprocal-rank fusion, and the sessions store is the same
`IndexStore` type the code index uses. `search_sessions` was simply calling
`search` (vector-only) instead. The fix was swapping the call.

That is the shape of this whole inconsistency, and worth remembering before
building anything from this document: the capability was present and one
caller had not been moved onto it.

## P9. Hook bridges

*DSH: [`packages/hooks`](https://github.com/deepseek-ai/deepseek-harness/tree/main/packages/hooks)*

DSH reads Claude Code's and Codex's existing hook configuration and runs those
shell hooks faithfully against its own interception points, keeping its native
typed extension surface as the real mechanism.

For Wingman this is an adoption play rather than a capability: a user with an
existing `.claude/settings.json` hooks block works on day one instead of
porting to `[hooks]`. Note the trust interaction — `[hooks]` is already one of
the sections ignored in an untrusted project config, and a bridge must inherit
that exact treatment rather than opening a side door around `wingman trust`.

**Size: M.**

## P10. Per-message feedback

*DSH: [`packages/feedback`](https://github.com/deepseek-ai/deepseek-harness/tree/main/packages/feedback)*

`docs/FEATURES.md` states the problem plainly: *"the outcome scoring behind
skill stats is a phrase heuristic over your replies, not a learned signal;
treat the numbers as a rough tally."*

DSH separates two things: an immutable `/feedback` remark in the session log,
and an editable per-message rating in a local sidecar. Neither enters model
context. A thumbs-up/down on an assistant message is a real signal, and it is
the one the learning loop currently lacks.

**Size: M.** TUI keybinding, sidecar store, and rewiring skill stats to prefer
explicit ratings and fall back to the heuristic.

## P11. Persistent PTY sessions

*DSH: [`packages/terminal`](https://github.com/deepseek-ai/deepseek-harness/tree/main/packages/terminal)*

Owner-scoped terminal sessions that hold state across tool calls, for REPLs,
interactive stdin, and anything needing a live shell. DSH is explicit that
this *complements* one-shot bash rather than replacing it — the one-shot tool
has stronger per-operation contracts and should stay the default.

Needs `portable-pty` or equivalent, plus readiness detection, bounded reads,
and a sandbox policy per session. Do it after P3, which establishes the
process-lifecycle plumbing.

**Size: L.**

## P12. Code Mode

*DSH: [`packages/code-runtime`](https://github.com/deepseek-ai/deepseek-harness/tree/main/packages/code-runtime)*

Rather than N round trips of tool-call JSON, the model writes one program
against a generated SDK:

```js
const hits = await tools.grep({ pattern: "TurnGate" })
for (const h of hits) await tools.read({ path: h.path })
```

Multi-step work collapses into a single request. It is the largest available
win on the metric Wingman headlines, and the largest lift: an embedded
runtime, a binding layer, a typed-return contract, and a genuine new security
surface — a model-written program that loops is a different risk than a model
that calls one tool at a time.

DSH treats it as *one optional capability, deliberately not part of the agent
loop spine*. Wingman should do the same: prototype behind a feature flag,
measure the token delta against the same tasks, and only then decide.

**Size: L–XL.**

---

# Part 3 — Engineering process

## E1. Decision records

DSH keeps 1484 markdown notes under
`.agents/notes/{proposed,implemented,archived,rejected}/{architecture,feature,bug-fix,simplification,testing}`,
and *every package README links to the note explaining why its boundary sits
where it does*. That second half is what makes them load-bearing rather than
an archive.

Wingman has one 108 KB `plan.md`. It has the information but not the shape: it
cannot be linked to from a specific decision, and it does not distinguish what
was decided from what was considered.

The underrated bucket is `rejected/`. It is what stops a plausible bad idea
being re-proposed every six months, and it is the bucket a `plan.md` never
has.

**Size: S to start.** Create the directory, add notes for the next decisions
rather than backfilling, and link from the docs that already exist.

## E2. Generated documentation

DSH generates `config-catalog.md`, `module-graph.md`, and `graph-atlas.md`
from source, with a `do not edit by hand` header and a regeneration command.
They cannot drift.

Wingman has 26 hand-maintained docs. `FEATURES.md` and `TOOLS.md` are the
drift-prone ones — both enumerate things that exist in code — and `TOOLS.md`
has already drifted. Seven tools ship in
`crates/wingman-tools/src/builtin/` with no entry in it:

`ask_user`, `command_tool`, `edit_symbol`, `lsp_tools` (which is itself five
tools), `outline`, `task_complete`, `update_tasks`.

Nothing is documented that doesn't exist — the drift is one-directional, which
is the expected shape: tools get added, the doc doesn't. `CONFIGURATION.md`
has the same exposure against the config structs.

Generate those three. Leave the prose docs alone — generation is right for
enumerations, wrong for explanations.

**Size: M** for the generator. Patching the seven missing entries by hand is
20 minutes and worth doing immediately regardless, since the generator is not
scheduled.

## E3. Runtime invariants

*DSH: [`runtime-diagnostics/invariants`](https://github.com/deepseek-ai/deepseek-harness/tree/main/packages/runtime-diagnostics)*

A registry of self-checks that packages own and register, toggleable by
allowlist. Their central one is the A1 invariant: anything model-visible must
be reconstructable from the session log.

Wingman wants that same check, for the same reason, and it is what keeps A1
true after the refactor that establishes it. Debug builds assert; release
builds skip.

**Size: M**, and it should land with A1 rather than after it.

---

# Suggested sequencing

**First — independent wins. Done.** P2 (repeat guard), P5 (deadlines),
P6 (presets), P8 (session search) and the seven missing `TOOLS.md` entries
landed together; P7 turned out to already exist. E1 (decision records) is
still open.

**Second — the context work. Done.** P1 (spill) and P4 (pruning) landed
together. They compose as expected: pruning a *spilled* result is lossless,
because the locator line sits in the head that pruning always keeps, so the
full text stays one `read_file` away no matter how far the result is later
shrunk.

**Third — the architecture. Now due.** A1 (event-sourced log) with E3
(invariants). P1 and P4 were named as the forcing function and both have now
landed, each adding model-visible state the log does not record: the spill
locator line and the pruned tool result. The session log is now behind the
model's context in three ways rather than one (recap, injected system text,
and these). A2 falls out incrementally.

**Fourth — capability gaps.** P3 (jobs), then P11 (PTY) on the plumbing P3
establishes. Trigger for P3 is someone actually hitting the 600s ceiling, not
a calendar. P9 and P10 slot in wherever they fit.

**Ongoing.** E2 (generated docs) whenever a drift is noticed. P12 (Code Mode)
as a flagged experiment, not a roadmap commitment.

**Not doing:** A3. Revisit only if third-party extensions become a goal, and
then as a WASM host with an explicit capability model rather than as dynamic
linking.
