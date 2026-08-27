# 0003 — A spill locator is the first line of a tool result

**Status:** accepted
**Date:** 2026-08-27

## Context

Oversized tool output is truncated head/tail and the full text spilled to
`.wingman/spill/<session>/`, with a locator telling the model where the rest
is. The obvious place for that locator is the elision marker in the middle,
where the cut happened — that is where the information is missing.

## Decision

The locator is the **first line** of the tool result, not part of the elision
marker.

`ToolResultPruner` later rewrites long results to a head and a tail. A locator
in the middle is exactly what pruning discards, so the two features would have
composed into "the model is told there is more, until it needs to know". The
head always survives both.

The locator points at the spill file, and — for `@file` attachments — at the
original path instead, because there the file is already on disk and no copy
is needed.

## Consequences

- Spill and pruning are lossless *together*, not just individually.
- The elision marker no longer claims the full output is "in the session log",
  which was an instruction the model had no tool to act on.

## What would change this

A change to what pruning preserves. If it ever keeps something other than a
head and a tail, re-check that the locator is inside the kept region.
