# 0006 — Containment root and owning root are separate questions

**Status:** accepted
**Date:** 2026-08-27

## Context

Pilot workers run inside `<project>/.wingman/worktrees/<name>` and `cd` there
before doing anything else. A git worktree is marked by a `.git` **file**, not
a directory, and `find_project_root` checks `.git`'s existence — so it stops at
the worktree.

That is correct for tool containment: a worker's edits belong to its own
branch, and confining `write_file` to the worktree is the point.

It is wrong for anything that must outlive the task. Worktrees are
force-removed by `cleanup_worktrees`, so a worker's session log written under
`paths.root` would be deleted along with the evidence of what produced it —
which is how the fix for the missing worker transcript would have quietly
failed.

## Decision

Two functions for two questions.

- `find_project_root` — unchanged. The containment root. Stops at a worktree.
- `find_owning_project_root` — looks through a worktree to the project that
  owns it. For artifacts that must survive the task.

The worktree is recognised by the two path components above it
(`.wingman/worktrees`), not by its own name, so a project that happens to be
called `worktrees` is unaffected.

## Consequences

- Worker transcripts land in `<project>/.wingman/sessions/`, which is what
  `AgentRecord.session_id` already claimed and `wingman session fork` needs.
- Any future per-task artifact has to pick a root deliberately. That choice is
  now visible in the function name rather than implied.

## What would change this

Worktrees moving outside `<project>/.wingman/`, which would break the
component check. It is asserted by a test that builds the real layout,
`.git` file included.
