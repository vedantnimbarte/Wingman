# 0005 — The repeat guard counts denied calls, and exempt tools are transparent

**Status:** accepted
**Date:** 2026-08-27

## Context

The guard watches for runs of identical consecutive tool calls and injects an
advisory at configured thresholds. Three details decide whether it works at
all, and all three are easy to get wrong in the obvious direction.

## Decision

**Denied calls count.** The guard sits *after* the registry's capability gate,
so a call the permission mode keeps refusing still increments the chain. A
model hammering a denied call is precisely the loop worth breaking; counting
only successful calls would miss the worst case.

**Exempt tools are transparent, not resetting.** An excluded call neither
increments the counter nor clears it, so `grep X → update_tasks → grep X` still
reads as two consecutive `grep X`. Resetting on exempt tools would let
bookkeeping interleaved into a loop launder it — which is the case exemption
exists to handle.

**Arguments are canonicalized by explicit key-sort**, not by trusting
`serde_json`'s `BTreeMap`. That ordering is a default-feature accident: any
crate in the dependency graph enabling `serde_json/preserve_order` flips it to
insertion order for the whole build, and the guard would silently stop
matching.

## Consequences

- One chain per `ToolRegistry`, and subagents build their own, so per-agent
  keying is free.
- In-memory only: a resumed session starts fresh. The guard is a heuristic
  nudge, and a few extra reminders are cheaper than persisting it.

## What would change this

Sharing one registry across parent and subagent. The chain would then need
per-agent keying explicitly.
