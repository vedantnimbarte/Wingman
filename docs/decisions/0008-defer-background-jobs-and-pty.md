# 0008 — Background jobs and persistent PTY wait for the ceiling to bite

**Status:** superseded by [0011](0011-background-jobs.md) for jobs; PTY still deferred
**Date:** 2026-08-27

## Context

`run_shell` blocks the turn and is capped at 60s by default, 600s maximum.
That rules out dev servers, watch processes, long test suites, and cold builds
of a large workspace. DSH solves it with a job registry (`start/poll/stop/wait`
plus completion notices) and a separate persistent-PTY capability for REPLs and
interactive stdin.

Both are real capabilities Wingman lacks, and the job protocol would also make
`spawn_subagent` non-blocking for free, since delegation is the same shape.

## Decision

Defer. Neither is built.

The 600s ceiling is real but nobody has hit it in practice yet, and building a
process table, an owner-fenced id scheme, completion delivery, and a PTY
backend on speculation is a lot of surface to maintain for a limitation that
has not yet cost anything. Sequencing also matters: jobs establish the
process-lifecycle plumbing that PTY should be built on, so doing PTY first
would mean building it twice.

## Consequences

- Long-running work is done by splitting it or raising `timeout_secs` to the
  600s maximum.
- If this is picked up, `run_shell(background: true)` returning a job id plus
  `job_output` / `job_kill` / `job_list` is the lazy shape — a process table
  and three tools, not a capability seam. Build it on a shared spawn seam so
  sandbox policy is not copied per backend.

## What would change this

Someone actually hitting the 600s ceiling in normal use, or a concrete need to
drive a REPL or dev server across tool calls. That is the trigger — not a
calendar.
