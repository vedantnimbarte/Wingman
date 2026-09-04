# 0017 — Desktop notifications are a file inbox, and approvals bypass it

**Status:** accepted
**Date:** 2026-09-05

## Context

Wingman stops and waits for a human in several places — a plan hits the
approval gate, `ask_user` wants a decision, a task fails — and until now those
moments were reachable only from the one terminal that happened to be in front
of you. `ask_user` returned "no interactive terminal — proceed with your best
judgment" the moment there was no TTY, so in a detached `pilot run`, a worker,
or a `serve`-hosted turn the agent simply guessed.

The desktop popup that fixes this has to be reachable from **every** surface: a
detached run with no relationship to the popup's PID, a worker subprocess, the
ratatui TUI, `wingman --print`, and a `serve` child. Two candidates were
rejected before the file:

- **A local HTTP listener in the popup.** Every raiser would need the port, a
  token, and a retry policy; the popup would need to bind, and binding fails
  when two are open. It also makes a notification depend on something being up,
  when the whole point is to record that a human is needed *whether or not*
  anyone is watching.
- **A socket** (Unix domain / named pipe). Same coupling, plus a per-platform
  implementation, and nothing to read when the popup is closed.

## Decision

Two append-only JSONL files under `~/.wingman/`, exactly the shape
`control.jsonl` already has for live runs: any process appends a line, the
interested one tails it by byte offset.

```
notifications.jsonl        any process appends; the popup reads
notification-replies.jsonl the popup appends; the asker tails
notifier.alive             the popup re-stamps this while it is running
```

Three consequences are load-bearing and will look arbitrary later:

**The inbox lives in `wingman-config`.** It looks like the wrong crate — a
config crate holding an IPC channel — and it is not. `wingman-tools` needs it
for `ask_user` and does *not* depend on `wingman-autonomous`; the dependency
runs the other way, as that crate's own doc records for `child_process`.
`wingman-config` is the lowest node both can see, it already owns the
`~/.wingman/` layout in `paths.rs`, and it already has serde. The alternative
was a new crate for ~250 lines, which [0001](0001-no-plugin-architecture.md)
and [0013](0013-no-speculative-seams.md) both argue against. This is the same
trade [0013](0013-no-speculative-seams.md) accepted for `wingman-board` →
`wingman-rag`.

**Approvals deliberately do not use the reply file.** A notification carries the
run directory, and an Approve button carries the literal `ControlCommand` JSON.
The popup appends it straight to `<run_dir>/control.jsonl` — the file
`wait_for_approval` and `run_notify_window` are already tailing. So the approval
gate needed a **zero-line change** on the run's side and ships covered by the
tests that were already there, and any future `ControlCommand` becomes a button
without the popup learning the vocabulary. A reply-then-translate design would
have added a consumer, a race, and a second record of a decision already taken.

**`ask_user` tries the popup before stdin, not after.** The obvious order is
TTY-first, and it is wrong here: the TUI holds the terminal in raw mode under an
alternate screen, so `stdin().is_terminal()` is true there while a blocking
`read_line` fights crossterm for keystrokes and writes its prompt to an
invisible stderr. Popup-first fixes that case too. What keeps it honest is the
liveness marker: with the feature on but no popup running, the route falls
through to the terminal prompt rather than sitting out a timeout nobody will
answer.

## Consequences

- Both files grow without bound. Readers are O(new bytes) via byte offset and
  expired entries are filtered on read, so this is a disk-space ceiling — a few
  hundred bytes an event — not a correctness or latency one. Marked in the code
  with the trigger for adding compaction.
- Every trim scheme races an appender, which is why there is none. It is
  affordable because nothing loss-critical rides this channel: approvals go via
  `control.jsonl`, and a dropped reply degrades to `ask_user`'s existing
  timeout note.
- Appends must be **one** `write_all`, not `writeln!` — that issues a separate
  syscall for the newline, and `store.rs` already tolerates a torn final line
  because of it. A per-run control file gets away with it because one process
  appends at a time; this file is written by a detached run, its workers, a TUI
  and a `serve` child at once. There is a concurrency test that fails if anyone
  reintroduces it.
- Both files are created `0600` on unix. `run_dir` arrives as an absolute path
  inside a file, and acting on it means appending caller-chosen JSON to a
  caller-chosen location, so the popup also refuses any path that is not
  directly under a `.wingman/autonomous/` directory and does not already contain
  a `tasks.jsonl`.
- Everything is off by default: `[pilot.notifications].desktop_inbox = false`
  and `[tools].ask_user_desktop_timeout_secs = 0`. With the defaults, the files
  are never created and `ask_user` behaves exactly as it always has.
