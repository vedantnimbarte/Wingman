# Decision records

Short notes on decisions whose *reasoning* is not visible from the code, kept
so the next person — or the same person in six months — does not have to
re-derive it or, worse, quietly undo it.

## When to write one

A record earns its place when at least one of these is true:

- **The boundary is arbitrary-looking but load-bearing.** Someone reading the
  code would reasonably ask "why not the other way?" and the answer is not in
  the diff.
- **It was rejected.** A plausible idea that was considered and turned down is
  the most valuable thing here, because nothing else in the repo records it and
  it *will* be proposed again.
- **It was deferred, with a trigger.** "Not yet, and here is what would change
  that" beats an untracked intention.

## When not to

[CONTRIBUTING.md](../../CONTRIBUTING.md) asks you to explain the why in the
commit message, and that remains the default — a record is not a substitute
for a good one. Skip a record for an ordinary bug fix, a refactor whose
motivation is obvious from the diff, or anything a comment next to the code
would answer better. A record you have to hunt for is worse than a comment you
cannot miss.

Aim for a page. If it is longer than the change it explains, it is the wrong
shape.

## Status values

| Status | Meaning |
|---|---|
| `accepted` | In force. |
| `rejected` | Considered and declined. Kept so it is not re-proposed blind. |
| `deferred` | Not now. The record names the trigger that would change it. |
| `superseded by NNNN` | Replaced. The newer record explains what changed. |

A record is not edited to reverse a decision — it gets a status change and a
pointer. The history of what we believed is the point.

## Index

| # | Decision | Status |
|---|---|---|
| [0001](0001-no-plugin-architecture.md) | Wingman stays a fixed binary rather than a plugin runtime | rejected |
| [0002](0002-loop-owns-the-session-log.md) | The agent loop is the only writer of the session log | accepted |
| [0003](0003-spill-locator-goes-in-the-head.md) | A spill locator is the first line of a tool result, not part of the elision marker | accepted |
| [0004](0004-pruning-is-idempotent-by-construction.md) | Tool-result pruning is bounded so a second pass is a no-op | accepted |
| [0005](0005-repeat-guard-counts-denied-calls.md) | The repeat guard counts denied calls and treats exempt tools as transparent | accepted |
| [0006](0006-two-project-roots.md) | Containment root and owning root are separate questions | accepted |
| [0007](0007-bound-the-read-not-the-result.md) | Attachment and output caps bound the read, not just the result | accepted |
| [0008](0008-defer-background-jobs-and-pty.md) | Background jobs and persistent PTY wait for the 600s ceiling to actually bite | deferred |
