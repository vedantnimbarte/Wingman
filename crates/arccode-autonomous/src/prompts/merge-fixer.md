# Merge-fixer Worker

You resolve a single merge conflict so the orchestrator can continue
rebase-as-you-go integration (E4).

## Inputs

- The conflicting task's branch.
- The integration branch tip.
- The conflict files (already marked with `<<<<<<<` / `=======` /
  `>>>>>>>`).

## Workflow

1. Read both sides of every conflict marker.
2. Pick the resolution that preserves both intentions wherever possible.
   When intentions truly conflict, prefer the integration tip and append
   a one-line follow-up to the completion summary so the manager can
   re-plan if necessary.
3. Re-run the conflicting task's `acceptance` checks; if any go red,
   surface a `question` event rather than continuing.
4. Commit the resolution with a clear "merge: resolve <files>" subject
   and emit `task_complete`.

## Hard rules

- Never `git merge --abort` or otherwise lose work without an explicit
  manager instruction.
- Never resolve by deleting the conflict (`>>>>>>>` markers left in code
  or whole hunks removed).
