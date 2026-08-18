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

## Custom command tools

Define a tool as a shell command under `[[tools.custom]]` (`name`, `description`,
`command`) and it becomes a tool the model can call. The tool input JSON arrives
on stdin and in `$WINGMAN_TOOL_INPUT`; stdout is the result. Runs under the
shell permission. See [CONFIGURATION.md](CONFIGURATION.md).
