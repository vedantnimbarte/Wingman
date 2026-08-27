# 0011 — Background jobs are a process table, not a capability seam

**Status:** accepted
**Date:** 2026-08-27
**Supersedes:** the jobs half of [0008](0008-defer-background-jobs-and-pty.md)

## Context

[0008](0008-defer-background-jobs-and-pty.md) deferred this and named the
trigger: someone actually needing it. That happened. The 600-second `run_shell`
ceiling rules out dev servers, watch processes, and cold builds of a large
workspace.

0008 also predicted the shape — "`run_shell(background: true)` returning a job
id plus `job_output` / `job_kill` / `job_list` is the lazy shape — a process
table and three tools, not a capability seam. Build it on a shared spawn seam
so sandbox policy is not copied per backend." That held up, with one
correction: the tool is `job_stop`, because it stops a tree rather than
signalling one process.

## Decision

### One preparation path, two ways to wait

`run_shell`'s preamble — permission mode, project denylist, cwd resolution,
sandbox wrapping, and scrubbing credentials out of the child's environment —
is extracted into `prepare()` and used by both paths.

A second copy would be a second place for the sandbox policy or the denylist
to be subtly wrong, and only one of them would have been tested. A background
command is not a less-guarded command; it is the same command, not waited on.

### `child_process` moved down rather than reimplemented

Killing a background job means killing its tree: `sh` alone leaves `npm`, and
`npm` leaves `node`. `wingman-autonomous::child_process` already solved this on
both platforms — process groups on Unix, Job Objects on Windows — for pilot
workers.

`wingman-tools` cannot depend on `wingman-autonomous` (that is the direction
the dependency already runs), so the module moved *down* into `wingman-tools`
and is re-exported from its old path. Process supervision has no pilot
concepts in it; it was simply in the first crate that needed it. Three usage
sites, and the alternative was a second tree-kill implementation that would
have drifted.

### Output is bounded and tail-biased

A dev server prints forever. Buffering all of it is the unbounded-growth
mistake that `@file` attachments ([0007](0007-bound-the-read-not-the-result.md))
and tool output ([0003](0003-spill-locator-goes-in-the-head.md)) each had to
fix, and here it would run for hours.

The buffer keeps the most recent 128 KiB, because a build's errors and a
server's latest request are both at the end, and says how much it dropped —
a truncated log that reads as complete is worse than one that admits it.

No spill file, unlike tool output: a job's history is unbounded by nature, so
there is no "full text" to point at.

### The table is shared with subagents, and dies with the session

`ToolCtx` is cloned into subagents, and the table is shared rather than
per-clone: a job started by delegated work belongs to the session, not to a
child that is about to disappear. Giving children their own table would orphan
whatever they left running.

`JobTable::drop` kills everything still alive, so a forgotten dev server does
not outlive the agent that started it.

## Consequences

- `job_output` declares `READ`, not `SHELL`. The command was gated when it was
  started; re-gating the read would mean a mid-session `/mode read-only`
  strands a running job's output where it cannot be collected.
- Jobs are in-memory. A restarted Wingman does not adopt jobs from a previous
  process — it also cannot kill them, which is the argument for the drop.

## What would change this

Persistent PTY ([0008](0008-defer-background-jobs-and-pty.md), still deferred)
is the next thing to build on this plumbing, and should reuse `prepare()` and
`child_process` rather than growing a third spawn path.
