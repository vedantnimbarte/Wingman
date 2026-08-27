# 0012 — Drive processes over stdin; no PTY

**Status:** accepted
**Date:** 2026-08-27
**Supersedes:** the PTY half of [0008](0008-defer-background-jobs-and-pty.md)

## Context

DSH ships a persistent-PTY capability, and [0008](0008-defer-background-jobs-and-pty.md)
deferred the equivalent. Its stated purpose is "workflows that require state
across tool calls or interactive stdin" — holding a REPL open, answering a
prompt, driving something whose next step depends on what it printed.

With background jobs ([0011](0011-background-jobs.md)) in place, that purpose
splits in two:

1. **State across tool calls and interactive stdin.** A job whose stdin stays
   open does this.
2. **Terminal semantics** — the child sees a TTY, so `isatty` returns true.
   This is what a PTY adds beyond a pipe.

## Decision

Build (1). Do not build (2).

`job_send` writes a line to a running job's stdin, so a process can be driven
across tool calls: start it, read what it said, answer, read again. That is the
capability, and it reuses the job table, `prepare()`, and `child_process`
rather than adding a spawn path.

A real PTY needs a crate (`portable-pty` or equivalent) pulling in ConPTY
bindings on Windows and its own transitive tree, against 525 crates already and
a `deny.toml` that reviews additions. What it buys is behaviour behind
`isatty`, and for an *agent* that behaviour is mostly unwanted:

- **Colour and progress bars.** ANSI escapes and spinner redraws are noise in a
  context window. The non-TTY path already gives the clean output we want.
- **Pagers.** `git log` piping into `less` and waiting for a keypress is a
  hang, not a feature.
- **Full-screen TUIs** (`vim`, `top`). An agent driving these by writing
  keystrokes is not a workflow worth enabling.

The remaining honest gap is REPL prompt echo: `python` and `node` on a pipe do
not print `>>>`. That costs nothing — the agent knows what it sent.

## Consequences

- `job_send` declares `SHELL`, not `READ`. Writing to a live process's stdin
  can make it do anything the process can do, unlike reading its output.
- Sending to a finished job is an explicit error naming the state, rather than
  a silent write into a closed pipe.
- A newline is appended when the caller omits one. Line-buffered programs —
  which is most of them, and every REPL — will not act until they see one, and
  a job that looks hung for that reason is a bad way to learn it.
- Programs that *require* a TTY do not work. That is the accepted cost.

## What would change this

A concrete workflow that needs `isatty` to be true and is worth an agent
doing. "A REPL doesn't print its prompt" is not that. If it arrives, build the
PTY behind the same `JobTable` interface — `start`/`send`/`output`/`stop` are
the right shape for both, so it is a backend swap rather than a second
subsystem.

## Note on testing

The obvious test — send input, wait for echoed output — measures the *child's*
stdout buffering, not this feature: `findstr` block-buffers on a pipe and `cat`
full-buffers, so both look like failures. The test instead uses a process that
reads two lines and exits, and asserts it is still running after the first send
and finished after the second. That is the property being claimed, and it does
not depend on anyone else's libc.
