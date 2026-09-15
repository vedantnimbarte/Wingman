# 0021 — Sandboxed pilot work comes back as a patch, and Docker is not a VM

**Status:** accepted
**Date:** 2026-09-15

## Context

J11 classified pilot tasks into `host`, `container` and `vm` tiers long before
anything ran them there. Two shortcuts were sitting in the code: when Docker
was reachable a `vm` task was reported as `vm`, and the obvious way to run a
container worker is to bind-mount its git worktree.

## Decision

- **A worker never touches the host worktree from inside a sandbox.** It runs
  against a copy (no `.git`), and the only thing that comes back is a
  `git diff --binary` ending in a completion line. The host applies it with
  `git apply`, excluding `.wingman/` and `.wingman-sandbox/`, and only when the
  worker passed the normal completion gate.
- **A VM's patch is read off a raw drive**, not out of a guest filesystem the
  host would have to mount and parse.
- **Docker never satisfies the `vm` tier.** A `vm` task with no Firecracker
  backend is refused, at plan time and when the manager adds one mid-run,
  unless `allow_unsandboxed_vm_tasks` says otherwise.
- **The host does not re-run a sandboxed task's acceptance checks.**

## Why not bind-mount the worktree

A mounted `.git` lets the sandboxed process rewrite hooks, config and refs that
the host's next `git` call executes or trusts. That turns "ran in a container"
into "ran on the host, one step later". A patch is inert data with a narrow
parser (`git apply`) that already refuses `.git` paths and symlink traversal.
The cost is recorded in PILOT-MODE.md: the worker's commit messages and its
`.wingman/` transcript do not come back.

## Why Docker is not a VM

A `vm` task is one the classifier judged hard to undo — migrations, infra,
Dockerfile or terraform edits. A container shares the host kernel; calling it
a VM in the run report told the operator a task had an isolation it did not
have. Failing closed is the only honest default; the opt-out exists and says
what it does.

## Why acceptance is self-reported

Acceptance commands for a sandboxed task are, by construction, the commands
the classifier did not want on the host (`npm install`, `curl`, build scripts).
Re-running them on the host to confirm the worker's claim would undo the
sandbox. The squash-merge and any later reviewer or critic still see the
applied diff.

## Consequences

Neither backend has run against a real Docker daemon or Firecracker host at the
time of writing; both are tested with mock runners and real `git`. A real run
that finds the patch channel too narrow should widen what the patch carries,
not reintroduce a mount.
