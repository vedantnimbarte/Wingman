# 0014 — The panel's diff is not green and red

**Status:** accepted
**Date:** 2026-08-27

## Context

The Changes screen renders the working-tree diff for a file, from `wingman
diff`'s own hunk-review output. Every diff anyone has ever seen — `git diff`
with colour on, GitHub, every editor's gutter — paints additions green and
deletions red, and `wingman diff` itself does too when it has a terminal. Not
doing that is conspicuous, and the first reaction to the screen is going to be
that its diff looks broken.

The panel's stylesheet opens with one rule, and it is the rule the whole design
is hung on:

> Colour encodes epistemic status, and nothing else. `--proven` is a
> verification that passed or a task that finished. `--asserted` is running and
> unproven. `--failed` is failed or blocked.

That rule is why a scan of the board answers the only question that matters:
what here is actually true, and what is merely running. It is also why cost —
the thing Wingman brags about most — deliberately gets no colour at all, only
typography and the ledger column.

A green `+` line and a red `−` line would be the first two exceptions. And they
would be *loud* exceptions: a diff is dense, so a screenful of it is a
screenful of `--proven` and `--failed`, on a screen where neither word applies.
A removed line is not a failure. An added line is not proven — proving it is
what the verification gate is for, two screens away.

## Decision

The two sides are told apart by **ground and gutter**, not by hue:

| | ground | gutter | ink |
|---|---|---|---|
| addition | `--raised` | `+` | `--ink` |
| deletion | `--sunken` | `−` | `--muted` |

This is not a novel treatment invented for the diff. It is the same
two-channel rule every status in the panel already follows — the one that says
a state must never be carried by colour alone, because three hues that all mean
"state" is where a colour-blind reader loses the board. The diff just uses the
second channel without the first.

### Alternatives considered

**Green and red, and treat the palette rule as applying to status only.**
Rejected. The rule's value is that it has no exceptions; a reader who has to
know *which* greens mean "proven" is a reader who no longer trusts any of them.
The exception would also be the largest block of colour in the product.

**A fourth and fifth hue, reserved for diffs.** Rejected. Five hues is not a
palette with a rule, it is a palette. And the two new ones would be read as
status by anyone arriving from the board, because that is what colour means
everywhere else in the panel.

**Ship no diff renderer; keep the raw text block.** Rejected on its merits
rather than on palette grounds — `diff`, `explain`, `review` and `attest` all
answer one question and had no home, and a monospace wall is not a rendering.
The raw block was also actively wrong here: `wingman diff` emits ANSI escapes,
so untouched it rendered `[32m` at the head of every added line.

**Keep the terminal's own colours by translating the ANSI escapes to spans.**
Rejected, and it is the version that looks cleverest: the CLI already decided
green and red, so the panel could simply honour them. That would import the
terminal's palette into a stylesheet with a different rule, and make the
decision above depend on a flag in a subcommand nobody would think to look at.
The escapes are stripped instead.

## Consequences

- The diff does not look like a diff at first glance, and that is a real cost
  paid on every first visit. The gutter glyphs are what make it legible in the
  second glance, so they are not decoration and must not be dropped for density.
- `--raised` and `--sunken` were already load-bearing for depth (a card on a
  page, an input in a card). Using them for diff sides means a future change to
  either token moves both, which is a coupling worth knowing about.
- Under `prefers-contrast: more` the two grounds stay as they are, because the
  gutter carries the distinction and raising the tints would fight the ink.

## What would change this

Nothing about taste. The rule would have to go first — if colour ever stops
meaning epistemic status in this panel, this record is moot and the diff should
look like every other diff.
