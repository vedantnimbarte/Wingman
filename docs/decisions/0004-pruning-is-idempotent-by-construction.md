# 0004 — Tool-result pruning is bounded so a second pass is a no-op

**Status:** accepted
**Date:** 2026-08-27

## Context

Under token pressure, oversized tool results in older turns are shrunk to
head + marker + tail before compaction folds whole turns away. Pruning runs on
every over-budget turn, so it runs many times over the same history.

## Decision

Validate that `head_chars + marker + tail_chars < threshold_chars`, and do
nothing at all when that does not hold.

A pruned result is then strictly under the threshold, so the next pass finds
nothing to prune. Without the property, pruning would rewrite the same results
every turn — churning history, and invalidating the cached prompt prefix each
time, which costs more than the pruning saves.

A configuration that cannot shrink refuses rather than silently correcting
itself: quietly adjusting someone's numbers hides the fact that they asked for
something impossible.

## Consequences

- `keep_recent` protects the results the model is actively working from.
  Pruning what it just asked for makes it ask again.
- Pruning is recorded ([0002](0002-loop-owns-the-session-log.md)) so a resumed
  session is not silently longer than the one it continues.

## What would change this

Making the marker dynamic (e.g. including the byte count). Its length is part
of the inequality, so a variable-length marker needs the bound computed from
its maximum, not its typical size.
