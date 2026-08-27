# 0002 — The agent loop is the only writer of the session log

**Status:** accepted
**Date:** 2026-08-27

## Context

Each surface used to compose session records by hand, from whatever it happened
to keep, and they disagreed. The TUI recorded the user prompt and the streamed
assistant text and nothing else — no tool calls, no tool results. `/resume` on
a TUI session therefore rebuilt a conversation in which the agent had never
used a tool, and `recall_session` indexed transcripts with all tool activity
missing. Headless recorded more; `serve` recorded differently again.

The root cause is that history lived in the loop while the log was written
outside it, so the two were structurally guaranteed to drift. The TUI even
documented the workaround: it recorded turn-locally *because* compaction
rewrites history and would invalidate any index into it.

## Decision

Hold one invariant: **model-visible means logged.** The loop is the only place
that knows what went into a request, so it is the only writer. At every point
it changes what the model will see it emits a `ContextFact`; a `ContextSink`
writes it down. Surfaces open the log and hand it over.

Deliberately kept off `AgentEvent`, which is the UI stream and a public
interface (`--print --json`). The two answer different questions — "what should
I show a human now" versus "what did the model receive" — and reusing it would
have changed a documented stream and duplicated assistant text within it.

A truncated tool result is recorded in both forms: `output` (full, the audit
trail) and `model_output` (what was actually sent). Replay uses `model_output`.

## Consequences

- Adding a model-visible input now means adding a `ContextFact`. That friction
  is the point.
- `debug_assert_reconstructs` fails the build's assertions if someone mutates
  history without recording it.
- Pilot workers gained a transcript ([0006](0006-two-project-roots.md)).
- Old logs still load: every addition is a new variant or a defaulted field.

## What would change this

Nothing short of the loop ceasing to be the single place a request is
assembled. If a second assembler appears, it needs the same sink, not its own
logging.
