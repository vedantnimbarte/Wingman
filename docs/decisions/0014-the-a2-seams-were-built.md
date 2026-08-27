# 0014 — The A2 seams were built; what that cost and bought

**Status:** accepted
**Date:** 2026-08-27
**Supersedes:** [0013](0013-no-speculative-seams.md)

## Context

[0013](0013-no-speculative-seams.md) closed A2 without building it, on the
grounds that a trait with one implementation is indirection that trades
compile-time dispatch for `dyn` and buys flexibility nobody has asked for. The
maintainer read that and asked for the sweep anyway.

It was built: three seams, each with a real second implementation, because the
second implementation was the specific thing 0013 said was missing.

| Seam | Implementations | Where |
|---|---|---|
| `SessionStore` | `FileSessionStore`, `MemorySessionStore` | `wingman-session` |
| `SkillStats` | `StatsStore` (SQLite), `MemoryStats` | `wingman-learn` |
| `FileSystem` | `OsFileSystem`, `MemoryFileSystem` | `wingman-tools` |

## What 0013 got right

Its factual claims held. There is still no remote filesystem, no non-JSONL
session store, and no second production backend for any of the three. Nothing
built here is swapped in production; `OsFileSystem`, `FileSessionStore` and
`StatsStore` are what every real session uses.

The four `Connection::open` sites are still four different databases, and a
storage seam over them still makes no sense. That part of A2 remains unbuilt
and should stay that way.

## What 0013 got wrong

**It equated "second implementation" with "second production backend."** A
test double is a real second implementation. It exercises the interface, it
catches interfaces shaped around a single caller, and — where the concrete
dependency is expensive or hazardous — it buys something specific:

- `StatsStore::open_default()` opens the user's **real** `~/.wingman/learn.db`.
  A test must never touch that, so the learning loop's own rules were
  effectively untestable. `MemoryStats` fixed a hazard, not an inconvenience.
- Skill-pattern extraction needed a temp directory and real JSONL files to
  exercise logic that never cared where sessions came from.

**It assumed the interfaces were knowable in advance.** Two of the three
changed shape during migration, and only because a real caller pushed back:

- `SessionStore` was written fully `async` for uniformity. The first consumer
  showed both implementations read *synchronously* — a file read and a map
  lookup — so async reads would have pushed `.await` and an async context onto
  every sync caller to buy nothing.
- `FileSystem` needed `read_blocking`, because `grep`, `find_symbol` and
  `who_calls` read inside `spawn_blocking` where no `.await` exists; and
  `Meta::modified`, because `recall_memory` cites when a memory was saved and
  could not otherwise migrate.

That is an argument *for* 0013's underlying instinct — an interface without a
second caller is guesswork — and against its conclusion, since building the
second caller is how you stop guessing.

## The benefit nobody predicted

The most valuable result of the filesystem seam is not swappability. It is
that **"every tool's file access goes through `ToolCtx`" became a property that
can be stated and tested.**

`no_tool_bypasses_the_filesystem_seam` scans `builtin/` and fails on any
production `std::fs::`/`tokio::fs::`. Before, that rule could not be written
down, let alone enforced; a new tool reaching for `std::fs` was invisible.
This has nothing to do with second backends, and waiting for one would never
have produced it.

## What it cost

- **Two implementations of the same rules** in `SkillStats`, and those rules
  are subtle (an inferred outcome must not overwrite a stated one; feedback
  attaches to the most recent unrated invocation in a window). Mitigated by a
  conformance script run against both — which is now the pattern all three
  seams use, and is the thing that makes duplication safe rather than merely
  cheap.
- **A less clean interface than the design implied.** `read_blocking` exists
  because reads genuinely happen in two contexts; pretending otherwise would
  have meant `block_on` inside a blocking closure or moving thousands of small
  reads onto the async runtime to satisfy an interface's shape.
- **`dyn` dispatch** on file reads and stat writes. Not measured; not expected
  to matter next to the I/O it wraps, and said plainly rather than assumed
  away.
- Roughly 1,500 lines across three PRs, none of it fixing a reported bug.

## Decision

Keep the three seams. Replace 0013's rule with a narrower one:

> Extract a seam when a second implementation would **buy something specific**
> — a production backend, a test double that removes a real hazard or a real
> cost, or an invariant that becomes enforceable. Build the second
> implementation as part of the same change, because that is what shapes the
> interface. Do not extract for symmetry, tidiness, or a diagram.

Under that rule these three qualify and a storage seam still does not.

## Consequences

- Every seam here ships a conformance test. A seam whose implementations
  disagree is worse than none, because callers are written against whichever
  one they happened to test with.
- The filesystem seam's scope is deliberately narrow and stated in its module
  docs: path containment and the audit log keep consulting the real
  filesystem, because "does this path escape the project" is a fact about the
  real one and a compliance trail a backend could redirect is not one.

## What would change this

Evidence that a seam is unused dead weight — no second implementation
exercised, no invariant enforced, no hazard removed. Delete it then; the
records exist so that removal is a decision rather than a guess.
