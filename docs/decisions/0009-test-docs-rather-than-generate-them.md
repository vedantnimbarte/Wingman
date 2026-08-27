# 0009 — Test the docs against the code rather than generating them

**Status:** accepted
**Date:** 2026-08-27

## Context

`TOOLS.md`, `CONFIGURATION.md`, and `FEATURES.md` each enumerate something the
code defines, and each had drifted:

- Seven tools shipped undocumented — `ask_user`, `edit_symbol`, `outline`,
  `update_tasks`, and the whole `lsp_*` family.
- `TOOLS.md` documented `glob_tool` and `grep_tool`. The tools are named
  `glob` and `grep`. Anyone following the docs got `unknown tool`.
- `@file` attachments were entirely undocumented, which is how a review of
  this codebase came to propose *building* a feature that already shipped.

The obvious fix, and the one originally proposed, is to generate these files
from source with a `do not edit by hand` header, the way DSH generates its
config catalog and module graphs.

## Decision

Do not generate. Assert instead.

These files are mostly prose — when to reach for a tool, what it costs, how it
fails, why a default is what it is. Generating them would replace explanation
with a schema dump, which is a worse document that happens to be accurate. The
part that drifts is the *enumeration* embedded in the prose, and that can be
checked without owning the writing:

- `every_tool_has_a_documented_section` / `every_tool_has_a_summary_table_row`
  — every registered tool has a heading and a table row.
- `the_documented_example_config_parses` — the entire example config from
  `CONFIGURATION.md` parses as a `Config`. The structs are
  `deny_unknown_fields`, so a documented key that does not exist is not a
  cosmetic bug: a user who copies the example has their *whole* config
  rejected, including the parts that were fine.
- `the_commented_presets_example_is_valid_when_uncommented` — commented-out
  examples exist to be uncommented, and a broken one is discovered by the user
  rather than by us.

A test fails loudly in CI, needs no generator to maintain, and leaves the
prose to a human.

## Consequences

- Adding a tool now fails the build until it is documented. That friction is
  the point; drift was previously invisible because nothing failed.
- Conditionally-registered tools are listed explicitly in the test, so a new
  one has to be added there. The forward check covers everything
  unconditional, which is where the drift actually happened.
- `FEATURES.md` is not covered. It is a curated narrative with no mechanical
  source of truth, and inventing one to check it against would be building the
  generator this decision declines.

## What would change this

A doc that genuinely is pure enumeration, with no prose worth preserving.
Generate that one; it is not these.
