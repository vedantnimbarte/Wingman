# 0013 — No speculative seams; extract when a second implementation arrives

**Status:** superseded by [0014](0014-the-a2-seams-were-built.md)
**Date:** 2026-08-27
**Closes:** A2 in [DSH-ADOPTION.md](../DSH-ADOPTION.md)

## Context

The DSH review listed four capabilities Wingman holds concretely where DSH has
a swappable service: session store, filesystem, subprocess/shell, and storage.
The proposed work ("A2") was to widen them into named seams.

Revisiting it with the code in front of me, three of the four have no driver:

- **Subprocess/shell — already done.** This was the one with a real second
  implementation, and background jobs ([0011](0011-background-jobs.md)) and
  stdin ([0012](0012-stdin-instead-of-a-pty.md)) delivered it: `prepare()` is
  shared by the foreground and background paths, and `child_process` moved
  down so both use one tree-kill.
- **Session store — nothing to extract.** Every reader (`wingman-learn`,
  the TUI, `distill`, `headless`) calls the *same two* functions,
  `load_session` and `records_to_messages`. That is already one
  implementation shared by all callers; a trait around it would add `dyn`
  indirection and no second backend.
- **Filesystem — no second implementation.** DSH's motivation is pointing the
  filesystem and subprocess providers at a remote sandbox so Bash, PTY, and
  LSP move together. Wingman has no remote execution and none planned.
- **Storage — not one thing.** The four `Connection::open` sites are four
  *different databases with different schemas* (`learn.db`, the board
  registry, `index.db`, `sessions.db`). They are not duplicated logic, and a
  storage seam would not merge them.

> **Superseded.** The sweep was built anyway, at the maintainer's direction.
> [0014](0014-the-a2-seams-were-built.md) records what it cost and bought,
> including where the reasoning below was wrong. The factual claims here still
> hold; the conclusion drawn from them did not survive contact.

## Decision

Do not do the sweep. A trait with one implementation is indirection with a
cost and no benefit: it trades compile-time dispatch for `dyn`, and it makes
the code harder to follow in exchange for flexibility nobody has asked for.
That is the same argument [0001](0001-no-plugin-architecture.md) makes at
larger scale.

Extract a seam when the second implementation actually exists. That is what
happened with the shell.

## What the review did find

Chasing "storage" surfaced a real defect rather than a missing abstraction.

Three SQLite stores, and only `wingman-board` set `journal_mode = WAL` and a
busy timeout — its own module comment even noted that `learn.db` did not.
Under the default rollback journal a writer excludes readers, and with no busy
timeout the loser gets `SQLITE_BUSY` immediately instead of waiting.

That is not theoretical. `learn.db` is opened by the agent's learning hook for
a whole session, and `/feedback` and `/skill stats` open their own connections
and write through them — a second writer against a live first one, in one
interactive session. Several call sites discard the result (`let _ = …`), so
the symptom is a skill outcome or a feedback rating that silently fails to
persist. The index databases have the same shape: background indexing writes
while the foreground queries.

Fixed with one `wingman_rag::sqlite::open` used by all three, so the next
store cannot forget. It lives in `wingman-rag` because that is where the
database machinery already is; a new crate for twenty lines would be worse,
and `default-features = false` keeps the edge light.

## Consequences

- The A2 row is closed as "done where justified", not deferred. Reopening it
  needs a second implementation, not a tidier diagram.
- `wingman-board` gained a `wingman-rag` dependency. Conceptually odd —
  "rag" is retrieval — but the alternative was a third copy of two pragmas.

## What would change this

A concrete second implementation: remote/containerised execution needing an
`fs` seam, or a non-JSONL session store. Build the seam then, with the second
backend in hand so the interface is shaped by two real callers rather than
one imagined one.
