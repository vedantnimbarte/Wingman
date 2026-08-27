# 0001 — Wingman stays a fixed binary rather than a plugin runtime

**Status:** rejected
**Date:** 2026-08-27

## Context

[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) is built on
Cordis, where *everything* is a plugin: the model adapter, the tool registry,
the session log, and the agent loop itself are config rows in a runtime plugin
tree. Reviewing it against Wingman raised the obvious question of whether to
follow.

The case for it is real, and four properties fall out that Wingman cannot
currently match:

1. Third-party extension without a fork. MCP and `[[tools.custom]]` cover
   *tools*, but not a new provider, compaction strategy, or session backend.
2. Reversible registration, which is what makes live reconfiguration safe.
   Wingman needs this exactly once today — `/model` swapping — and does it by
   hand.
3. Per-session composition: one process running several differently-composed
   agents. Pilot mode wants this and approximates it with subprocesses.
4. One uniform interception point. Wingman has `LearningHook`, `TurnGate`, the
   registry's permission gate, and `[hooks]` as four unrelated mechanisms, and
   the spill / prune / repeat-guard / deadline work added more.

Point 4 is the strongest and is not hypothetical.

## Decision

Do not build a plugin runtime. Take the subsystems instead.

Cordis leans on TypeScript-specific machinery with no cheap Rust equivalent:
service-by-key lookup becomes a `TypeMap` of `Arc<dyn Any>` with downcasts,
losing compile-time checking exactly where Wingman has it; declaration-merged
event maps have no open-enum equivalent, so events land in a closed enum in a
core crate — reintroducing the privileged core the design exists to avoid;
runtime loading means an unstable ABI, a WASM host, or feature flags, and only
one of those is a plugin system.

The second-order cost is visible in DSH's own repo: a `--dump-config` command
to answer "what is running", a generated config catalog because configuration
is no longer readable from source, generated module graphs because the
dependency structure is no longer apparent, and a framework primer before a
contributor can make a first change.

The deciding argument is positional. Wingman's claim is a measured number
(`wingman context` → first-turn tokens). A plugin tree makes that a function of
installed plugins rather than a property of the binary, which is what every
other agent already says. A single static binary with a knowable tool set is
also most of the Windows and air-gapped story, and `wingman attest` would have
to reason about a runtime loader as a new egress channel.

The fuller comparison, including everything that *was* adopted, is
[DSH-ADOPTION.md](../DSH-ADOPTION.md).

## Consequences

- Extension stays: MCP (host and server), `[[tools.custom]]`, `[hooks]`,
  skills, and PRs.
- The four unrelated interception points remain unreconciled. If a fifth is
  needed, revisit — not the plugin tree, but consolidating the seam.
- [0002](0002-loop-owns-the-session-log.md) delivered properties 2 and 4 in
  narrow form for a fraction of the cost.

## What would change this

Third-party extensions becoming a *goal* — people shipping Wingman plugins
they do not want to upstream. Revisit then as a WASM host with an explicit
capability model, not as dynamic linking, and expect to pay the
"what is actually running" tax DSH pays.
