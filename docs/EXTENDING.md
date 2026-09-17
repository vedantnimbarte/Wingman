# Extending Wingman

Hooks, user-defined slash commands, and custom tools — all without recompiling.

User-defined shell hooks fire at four well-known points. Configure under
`[hooks]` in `config.toml`:

```toml
[[hooks.pre_tool_use]]
command = "cargo fmt --check"
match_tool = "edit_file"      # also matches "edit_file*" or "*"
block = true                  # exit != 0 cancels the tool call
timeout_secs = 10

[[hooks.post_tool_use]]
command = "echo \"$WINGMAN_TOOL_NAME ran\""

[[hooks.stop]]
command = "notify-send 'wingman done'"

[[hooks.user_prompt_submit]]
command = "grep -qiv secret <<< \"$WINGMAN_USER_PROMPT\""
block = true                  # reject prompts containing 'secret'
```

The agent loop populates per-event environment variables
(`WINGMAN_TOOL_NAME`, `WINGMAN_TOOL_INPUT`, `WINGMAN_TOOL_OUTPUT`,
`WINGMAN_TOOL_IS_ERROR`, `WINGMAN_STOP_REASON`, `WINGMAN_USER_PROMPT`).
Hooks run via `sh -c` on Unix and `cmd /C` on Windows, with the
configured `timeout_secs` (default 10).

Place markdown files at `~/.wingman/commands/<name>.md` (global) or
`<project>/.wingman/commands/<name>.md` (project). When the user types
`/<name> rest of line` in the TUI, the markdown body is expanded into the
prompt with the literal token `$ARGS` replaced by `rest of line`, and
submitted as if typed directly. Project-local commands shadow globals.

Example `~/.wingman/commands/refactor.md`:

```markdown
Refactor the following Rust code with these constraints:
1. Keep the public API unchanged.
2. Prefer iterators over explicit loops.
3. Run `cargo clippy` mentally and address obvious lints.

$ARGS
```

Then in the TUI: `/refactor crates/foo/src/lib.rs` expands to a complete prompt.

## Plugins

A plugin bundles commands, skills, hooks, and MCP servers into one install.
The format is Claude Code's plugin layout, not a Wingman variant, so a plugin
written for Claude Code installs as-is:

```text
my-plugin/
  .claude-plugin/plugin.json   {"name": "my-plugin", "version": "1.0.0", "description": "…"}
  commands/review.md           → /review, or /my-plugin:review
  skills/tidy/SKILL.md         → skill "tidy"
  agents/helper.md             → skipped (no Wingman equivalent)
  hooks/hooks.json             → hooks, Claude Code settings.json shape
  .mcp.json                    → MCP server plugin_my-plugin_<server>
```

```sh
wingman plugin install ./my-plugin
wingman plugin install https://github.com/you/my-plugin#v1.0.0
wingman plugin trust my-plugin     # only needed for hooks / MCP servers
```

It lands in `~/.wingman/plugins/<name>/`, and the command and skill loaders
search there directly rather than copying files into `~/.wingman/commands`.
Your own commands and skills, global or project, win over a plugin's of the
same name. `${CLAUDE_PLUGIN_ROOT}` is replaced with the install path in
commands, skills, hook commands, and MCP server definitions, and exported to
stdio MCP servers as an env var. A command's frontmatter is stripped, and
`$ARGUMENTS` works alongside `$ARGS`.

**Trust.** Commands and skills are prompt text and load as soon as the plugin
is installed. Hooks and MCP servers run programs, so they are inert until
`wingman plugin trust <name>`. That pins a SHA-256 of *every* file in the
plugin, not just `hooks.json` and `.mcp.json` — a hook is usually
`${CLAUDE_PLUGIN_ROOT}/scripts/x.sh`, and pinning only the JSON would let an
update swap the script under a still-trusted hook. Any change, including
reinstalling a different version, lapses trust. `wingman doctor` lists
installed plugins and flags untrusted or lapsed hooks/MCP. Install refuses a
plugin that contains a symlink, a manifest name outside `[A-Za-z0-9_-]`, or a
command named like a built-in (`/help`, `/clear`, …).

**What is not supported, and what has not been tried.**

- Only the default layout is read. A `plugin.json` that relocates components
  (`"hooks": "./hooks/other.json"`, `"commands": [...]`) is installed, but those
  keys are reported as skipped and ignored.
- `agents/*.md` subagents are listed and skipped: pilot roles are
  orchestrator prompts, not something the model spawns by name.
- Hook events Wingman lacks (`SessionStart`, `Notification`, `PreCompact`, …) and
  non-`command` hook types are skipped; install lists the events. Hooks get
  `${CLAUDE_PLUGIN_ROOT}` substituted in their command text, but not as an env
  var, so a script that reads `$CLAUDE_PLUGIN_ROOT` itself won't find it.
- `.mcp.json` servers of type `stdio` and `http` work; `sse` is skipped. Other
  `${VAR}` placeholders in an MCP server's env or headers (for example
  `Bearer ${GITHUB_PERSONAL_ACCESS_TOKEN}`) are **not** expanded — the same is
  true of `[mcp]` in config.
- Claude Code command extras — `` !`cmd` `` inline shell, `@file` references,
  `allowed-tools`, `$1`-style positional arguments — are not interpreted; the
  text is passed to the model as written.
- Nested `commands/<dir>/x.md` are not discovered. `#ref` must be a branch or
  tag, not a commit sha. Marketplaces (a repo of many plugins) are not
  supported; point `install` at one plugin's directory.
- Validated on Windows by installing four published plugins from a local
  Claude Code plugin cache: `plugin-dev` (commands, skills, agents skipped),
  `frontend-design` (skills), `github` (an `http` MCP server — installed and
  reported, never connected, and its token header would not expand), and
  `ponytail` (whose relocated `"hooks"` path was reported as skipped). No
  published plugin with a default-layout `hooks/hooks.json` has been installed,
  and no plugin hook or MCP server has been run live; that path is covered by
  tests only. `git` install was exercised against a local `file://` clone, not
  a hosted repository.

## Custom command tools

Define a tool as a shell command under `[[tools.custom]]` (`name`, `description`,
`command`) and it becomes a tool the model can call. The tool input JSON arrives
on stdin and in `$WINGMAN_TOOL_INPUT`; stdout is the result. Runs under the
shell permission. See [CONFIGURATION.md](CONFIGURATION.md).

Pilot workers can propose tools of the same shape for a project (tool
synthesis): they land in `.wingman/tools/<name>.toml` and load once approved
with `wingman pilot tools approve <name>`. See
[PILOT-MODE.md](PILOT-MODE.md#tool-synthesis).
