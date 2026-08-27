# 0010 — Claude Code hooks are imported opt-in, and translated

**Status:** accepted
**Date:** 2026-08-27

## Context

Someone arriving from Claude Code already has a working `hooks` block in
`settings.json`. Making them rewrite it as `[hooks]` before they can try
Wingman is a pointless tax — the two formats say the same thing in different
shapes.

Three decisions in this were not obvious, and each has a wrong answer that
looks reasonable.

## Decision

### Off by default

Hooks execute shell commands. Running another tool's configuration because it
happened to be on disk is a surprise with an arbitrary blast radius, and "it
was already there" is not consent. `[hooks].import_claude_code` defaults to
false.

The cost is that the feature is invisible, so `wingman doctor` reports when an
importable file exists *and actually declares hooks* — warning about a
`settings.json` that would import nothing is the kind of noise that teaches
people to ignore warnings.

### A project file is trust-gated; a global one is not

`~/.claude/settings.json` is the user's own. `<project>/.claude/settings.json`
is part of whatever repository was cloned, and is exactly as dangerous as
`<project>/.wingman/config.toml` — which Wingman already refuses to honour for
executable keys until `wingman trust`.

`trust::is_trusted` keys on file *content*, so it extends to this file with no
new mechanism, and trust lapses when the file changes. A bridge that skipped
this would be a side door around `wingman trust` that happens to be spelled
differently.

### Matchers are translated, not copied

Claude Code names tools `Bash`, `Read`, `Edit`; Wingman names them
`run_shell`, `read_file`, `edit_file`. Copying a matcher verbatim would import
cleanly and then never fire — worse than not importing, because it looks like
it worked.

The map covers the standard tools. Alternations (`Edit|Write`) become one hook
per tool, since `match_tool` has no alternation and dropping the rest would
lose more. `mcp__*` names pass through: both products namespace MCP tools the
same way. A matcher that is a real regex is kept verbatim **and reported**,
because a silently-never-matching hook is the exact failure this exists to
prevent.

## Consequences

- `block` is not a faithful round-trip. Claude Code distinguishes "fail" from
  "block" by exit code; Wingman's `Hook` has one boolean. `PreToolUse` and
  `UserPromptSubmit` import as blocking — failing closed is the right way to
  be wrong about a guard — and this is documented rather than hidden.
- Events Wingman lacks (`SessionStart`, `PreCompact`, …) are skipped rather
  than rejected. Refusing a whole file for mentioning one unsupported event
  would defeat the purpose.
- Malformed JSON imports nothing rather than failing startup. A broken file
  belonging to a different tool must not stop Wingman from running.

## What would change this

Wingman growing a richer `Hook` (an exit-code policy rather than a boolean)
would make the `block` mapping faithful and is the one real fidelity gap.
Codex hooks, if wanted, are a second translation table against the same
machinery — the bridge shape is not specific to Claude Code.
