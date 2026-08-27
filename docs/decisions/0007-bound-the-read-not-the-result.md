# 0007 — Caps bound the read, not just the result

**Status:** accepted
**Date:** 2026-08-27

## Context

`@file` attachments inlined whatever they found, via `read_to_string`. The
obvious fix is to truncate the string afterwards.

That fixes the context cost and leaves the memory hazard entirely in place:
`read_to_string` loads the whole file before anything can trim it, so a large
enough attachment exhausts memory before the request is even built.

## Decision

Bound the **read**. `read_bounded` pulls `limit + 1` bytes — enough to detect
that the file continued, without pulling in the rest of it.

Two consequences follow that a naive cap would have got wrong:

- **Truncation is not detectable by length.** Eliding a handful of very short
  lines makes the trimmed form *longer* than the input once a marker is added.
  A caller comparing lengths concludes nothing was lost and skips the spill on
  a result that did lose a line. Both paths go through one `would_trim`
  predicate instead of each deciding for itself.
- **Binary files must still be refused.** `read_to_string` rejected invalid
  UTF-8; the bounded read keeps that, tolerating invalid bytes only in the last
  few positions of a cut we made ourselves — a multi-byte character straddling
  the boundary, which is dropped.

Images are refused rather than truncated, checked from metadata before reading:
a partial image is not a smaller image, and an oversized one should not be
read, encoded, and uploaded only for the provider to reject it.

## Consequences

- Per-file and per-prompt budgets, because a per-file cap alone still lets
  `@a @b @c …` add up to the same problem.
- No config knob. A knob here only lets someone opt into a bad idea, and
  `read_file` with `offset`/`limit` is the right tool for a large file — which
  is what the truncation marker points at.

## What would change this

A genuine need to inline something larger than the caps. Prefer raising the
constant over adding a knob, and check the memory bound still holds.
