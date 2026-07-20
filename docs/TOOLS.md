# Built-in Tools Reference

Wingman provides a comprehensive suite of built-in tools. This document describes each tool's signature, behavior, permission requirements, and use cases.

## Overview

Tools are registered in `ToolRegistry` (`crates/wingman-tools/src/registry.rs`). When the agent calls a tool:
1. Registry looks up the tool by name.
2. Checks permission mode (read-only, plan, auto-edit, yolo).
3. Runs pre-tool hooks (if configured).
4. Executes the tool.
5. Truncates output per `tool_output_max_lines`.
6. Runs post-tool hooks.

Tools return `ToolOutcome`:
```rust
pub struct ToolOutcome {
    pub output: String,        // printed to user
    pub is_error: bool,
}
```

## File Operations

### `read_file`

Read a file and return its content with line numbers.

**Signature:**
```json
{
  "tool": "read_file",
  "args": {
    "path": "/absolute/path/to/file.rs"
  }
}
```

**Permission:** Always allowed (read-only mode).

**Returns:**
```
    1 | fn main() {
    2 |     println!("hello");
    3 | }
```

**Notes:**
- Absolute path required.
- `.ipynb` files are parsed; cells returned as fenced code blocks + markdown.
- Output truncated per `tool_output_max_lines` (head + tail).

**Example:**
```
User: "What does the main function do?"
Agent calls: read_file(path="/path/to/main.rs")
Output: [file contents with line numbers]
Agent: "The main function prints 'hello'."
```

### `write_file`

Create or completely overwrite a file.

**Signature:**
```json
{
  "tool": "write_file",
  "args": {
    "path": "/absolute/path/to/new_file.rs",
    "content": "fn main() { ... }"
  }
}
```

**Permission:**
- Inside project tree: `auto-edit` or `yolo` (allowed); `read-only` or `plan` (prompts).
- Outside project tree: always prompts.

**Returns:**
```
✓ Wrote 42 bytes to /path/to/new_file.rs
```

**Notes:**
- Creates parent directories if missing.
- Overwrites existing file without warning (destructive).
- For safe edits, use `edit_file` instead.

### `edit_file`

Make a safe, exact-match string replacement in an existing file.

**Signature:**
```json
{
  "tool": "edit_file",
  "args": {
    "path": "/path/to/file.rs",
    "old_string": "fn foo(x: i32) {\n    x + 1\n}",
    "new_string": "fn foo(x: i32) -> i32 {\n    x + 1\n}"
  }
}
```

**Permission:**
- Inside project tree: `auto-edit` or `yolo` (allowed); `read-only` or `plan` (prompts).
- Outside project tree: always prompts.

**Returns:**
```
✓ Replaced 1 occurrence in /path/to/file.rs
```

**Error cases:**
- `old_string` not found (exact match required).
- `old_string` matches multiple times (ambiguous).

**Notes:**
- Exact match required; use `read_file` to find the exact text.
- Use `\n` for newlines (raw string literals don't expand escapes).
- Reversible if version control is in use.

**Example:**
```
Agent reads file, finds:
    42 | fn add(a: i32, b: i32) {
    43 |     a + b
    44 | }

Agent edits with:
  old_string: "fn add(a: i32, b: i32) {\n    a + b\n}"
  new_string: "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}"
```

### `apply_patch`

Apply a multi-file edit atomically. Updates, adds, and deletes are all-or-nothing.

**Signature:**
```json
{
  "tool": "apply_patch",
  "args": {
    "patch": "--- a/file1.rs\n+++ b/file1.rs\n@@ -1,2 +1,3 @@\n...\n--- /dev/null\n+++ b/newfile.rs\n@@ -0,0 +1,5 @@\n..."
  }
}
```

**Format:** Unified diff (git-style).

**Permission:** Same as `edit_file` (per-file checks).

**Returns:**
```
✓ Applied patch: 3 files modified, 1 added, 0 deleted
```

**Error cases:**
- Hunk doesn't apply cleanly → entire patch rejected.
- Partial file permissions → partial patch rejected.

**Notes:**
- Atomic: all changes or none. Prevents partial-write bugs.
- Useful for multi-file refactors.

## Search & Discovery

### `glob_tool`

Find files matching a glob pattern.

**Signature:**
```json
{
  "tool": "glob_tool",
  "args": {
    "pattern": "**/*.rs"
  }
}
```

**Returns:**
```
crates/wingman-cli/src/main.rs
crates/wingman-cli/src/cli.rs
crates/wingman-core/src/lib.rs
…
```

**Notes:**
- `**` matches directories recursively.
- Searches from project root (or cwd if outside project).
- Results sorted by modification time.

**Examples:**
```
glob_tool(pattern="src/**/*.rs")     # all Rust files in src/
glob_tool(pattern="**/*.{js,ts}")    # JS and TS files
glob_tool(pattern="test_*.py")       # test files in current dir
```

### `grep_tool`

Search file contents using ripgrep semantics.

**Signature:**
```json
{
  "tool": "grep_tool",
  "args": {
    "pattern": "async fn.*Result",
    "glob": "**/*.rs",
    "context_lines": 2
  }
}
```

**Returns:**
```
crates/wingman-core/src/agent.rs:52:pub async fn stream(
crates/wingman-core/src/agent.rs:53:    request: &CompletionRequest,
crates/wingman-core/src/agent.rs:54-) -> Result<ProviderEventStream> {
…
```

**Parameters:**
- `pattern` — regex (case-insensitive by default; use `(?-i:...)` to force case-sensitive).
- `glob` — file pattern filter (optional; defaults to `**/*`).
- `context_lines` — lines before/after matches (default 0).

**Notes:**
- Full regex syntax supported.
- Results limited to first 50 (prevents token overflow).

**Examples:**
```
grep_tool(pattern="fn main")                      # find main functions
grep_tool(pattern="TODO|FIXME", glob="**/*.rs")   # all TODOs in Rust files
grep_tool(pattern="class\\s+\\w+", glob="**/*.py") # class definitions in Python
```

### `list_dir`

List directory contents.

**Signature:**
```json
{
  "tool": "list_dir",
  "args": {
    "path": "/path/to/dir"
  }
}
```

**Returns:**
```
wingman-cli/          (dir)
wingman-core/         (dir)
wingman-config/       (dir)
Cargo.toml            (file, 2.4 KB)
README.md             (file, 30.5 KB)
```

**Notes:**
- Absolute path required.
- Shows file sizes and type (dir/file).
- Sorted alphabetically.

## Shell Execution

### `run_shell`

Execute a shell command. Blocked by denylist on `auto-edit` mode.

**Signature:**
```json
{
  "tool": "run_shell",
  "args": {
    "command": "cargo build --release",
    "timeout_secs": 60
  }
}
```

**Returns:**
```
Compiling wingman v0.0.1
Finished release [optimized] target(s) in 42.5s
```

**Permission:**
- Inside project tree: `auto-edit` or `yolo` (allowed, unless in denylist); `read-only` (prompts); `plan` (denied).
- Outside project tree: always prompts (`yolo` auto-allows).

**Notes:**
- Runs in project root (or cwd if outside project).
- Timeout enforced (default 60s, max 300s).
- Stdout and stderr combined.
- On Windows, runs via `cmd /C`; on Unix, via `sh -c`.

**Common denylisted commands:**
- `rm -rf /`
- `git push`
- `git commit --amend`
- `dd if=/dev/sda`

## Web Tools

### `web_fetch`

Download a URL and return text (HTML stripped).

**Signature:**
```json
{
  "tool": "web_fetch",
  "args": {
    "url": "https://example.com/article"
  }
}
```

**Returns:**
```
# Article Title

This is the article content. Headings and text are extracted;
HTML markup is removed.
```

**Notes:**
- Follows redirects.
- Timeout: 10s.
- Returns plain text (not raw HTML).
- No authentication; public URLs only.

### `web_search`

Search the web using DuckDuckGo (no API key).

**Signature:**
```json
{
  "tool": "web_search",
  "args": {
    "query": "rust async error handling best practices"
  }
}
```

**Returns:**
```
1. Title: "Async Rust Patterns" (example.com)
   Snippet: "Error handling in async code requires careful consideration…"
   URL: https://example.com/async

2. …
```

**Notes:**
- Top 10 results returned.
- Pairs well with `web_fetch` (search, then fetch top result).
- No API key needed.

## Semantic Search

### `semantic_search`

Search the project RAG index for relevant code chunks.

**Signature:**
```json
{
  "tool": "semantic_search",
  "args": {
    "query": "how do we handle concurrent requests",
    "top_k": 5
  }
}
```

**Returns:**
```
1. File: crates/wingman-core/src/agent.rs (lines 120-150)
   Relevance: 0.87
   Symbol: AgentLoop::run
   Preview: pub async fn run(&mut self, request: ...) { … }

2. …
```

**Notes:**
- RAG index must be built (automatic on first run).
- Embedding is semantic (understands meaning, not just keywords).
- Results include filename, line range, and code snippet.
- `top_k` defaults to 5; max 20.

## Symbol Graph

Tree-sitter-backed navigation (feature `treesitter`, on by default). Answers
"where is this defined / who uses it" in one tool call instead of several
grep→read round-trips. Supported languages: rust, python, javascript,
typescript, tsx, go.

### `find_symbol`

Locate where a symbol is *defined* (not merely mentioned). Args: `name`
(required), `glob`, `limit`, `case_insensitive`. Returns
`path:line  kind  name  signature` rows.

### `who_calls`

Find *references* to a symbol (call sites, mentions), each annotated with the
enclosing function/method — the part `grep` can't tell you. Skips the
definition line itself. Args: `name` (required), `glob`, `limit`.

**Returns:**
```
crates/wingman-ts/src/parse.rs:257  [in fn semantic_chunks]  let symbols = extract_symbols(lang, src);
crates/wingman-ts/src/parse.rs:370  [in fn outline]          let symbols = extract_symbols(lang, src);
```

**Notes:**
- Whole-word, case-sensitive name match — a name-based heuristic, not resolved
  references, so it can over-report same-named symbols and miss dynamic calls.
- Pair with `find_symbol` (definition) for the full picture of a symbol.

## Planning & Structured Output

### `present_plan`

Create a structured plan block. Required before edits in `plan` mode.

**Signature:**
```json
{
  "tool": "present_plan",
  "args": {
    "title": "Refactor token estimation",
    "steps": [
      "1. Analyze current estimate_tokens() function",
      "2. Identify inefficiencies",
      "3. Design new algorithm",
      "4. Update callers",
      "5. Test with large histories"
    ],
    "estimated_time_minutes": 30
  }
}
```

**Returns:**
```
✓ Plan presented. Awaiting approval…
```

**Notes:**
- In `plan` mode: blocks all writes/shell until user approves.
- In other modes: logs the plan but doesn't block.
- User can approve with `yes` / request edits with `edit` / reject with `no`.

## Memory & Learning

### `save_memory`

Persist a fact, preference, or instruction across sessions.

**Signature:**
```json
{
  "tool": "save_memory",
  "args": {
    "name": "user-pkg-manager",
    "type": "feedback",
    "description": "Use pnpm for package management",
    "body": "Always use pnpm instead of npm. It's faster and more reliable."
  }
}
```

**Returns:**
```
✓ Saved memory: user-pkg-manager (feedback)
```

**Memory types:**
- `user` — facts about the human (scope: global).
- `feedback` — behavioral preferences (scope: global).
- `project` — facts about this codebase (scope: project).
- `reference` — external pointers (scope: global).

**Notes:**
- Creates `<scope>/memory/<slug>.md` and updates `<scope>/memory/MEMORY.md`.
- Next session: index rendered into system prompt; full body available via `recall_memory`.

### `recall_memory`

Fetch the full body of a memory by name.

**Signature:**
```json
{
  "tool": "recall_memory",
  "args": {
    "name": "user-pkg-manager"
  }
}
```

**Returns:**
```
Always use pnpm instead of npm. It's faster and more reliable.

When setting up a new project, run: pnpm init && pnpm install
```

**Notes:**
- Index appears in system prompt automatically; use when you need the full body.

### `forget_memory`

Delete a memory permanently.

**Signature:**
```json
{
  "tool": "forget_memory",
  "args": {
    "name": "user-pkg-manager"
  }
}
```

**Returns:**
```
✓ Deleted memory: user-pkg-manager
```

## Skills

### `invoke_skill`

Load a skill for the current turn. Records usage and outcome.

**Signature:**
```json
{
  "tool": "invoke_skill",
  "args": {
    "name": "refactor-rust"
  }
}
```

**Returns:**
```
Skill: refactor-rust

When refactoring Rust code, follow these steps:
1. Run cargo clippy and address lints
2. Use iterators where possible
3. Keep public API stable
4. Write tests for public functions
```

**Notes:**
- Loads from global and project skills (project shadows global).
- Outcome tracked in `learn.db` and scored on next turn.
- Skills auto-loaded at startup appear in system prompt (no need to invoke unless the agent wants the full body).

## Session Management

### `recall_session`

Search past sessions across projects for relevant context.

**Signature:**
```json
{
  "tool": "recall_session",
  "args": {
    "query": "how do we handle race conditions in tokio code"
  }
}
```

**Returns:**
```
1. Project: Wingman (2024-05-27)
   Relevance: 0.91
   Session: 2024-05-27-143015.jsonl
   Snippet: "We use tokio::sync::Mutex for shared state…"

2. Project: MyServer (2024-04-15)
   Relevance: 0.78
   Session: 2024-04-15-090500.jsonl
   Snippet: "Message channels with select! prevent race conditions…"
```

**Notes:**
- Searches `~/.wingman/sessions.db` (embeddings of past sessions).
- Cross-project; helpful for pattern recall.
- Top 5 results returned.

### `read_session`

Fetch the full JSONL transcript of a session.

**Signature:**
```json
{
  "tool": "read_session",
  "args": {
    "session_id": "2024-05-27-143015"
  }
}
```

**Returns:**
```
[Full JSONL conversation log]
```

**Notes:**
- Used after `recall_session` returns relevant session ID.
- Enables "pattern replay" from past work.

## Subagent Control

### `spawn_subagent`

Run an isolated inner agent on a sub-task. Depth capped at 1.

**Signature:**
```json
{
  "tool": "spawn_subagent",
  "args": {
    "task": "Fix the failing test in test_edit_file",
    "description": "You are a testing expert. Be concise.",
    "model": "",
    "task_class": "search"
  }
}
```

`task_class` (optional) drives model routing: classified subagents
(`search`, `summarize`, `codegen`, `reason`) resolve their model through
`[router.classes]` in config, so lookup/condense work can run on a cheaper,
faster model while the parent session keeps the strongest one. An explicit
`model` override always wins; empty class inherits the session model.

**Returns:**
```
✓ Subagent completed: Identified off-by-one error in line 42 of tests/edit.rs.
  Suggested fix: change `end_line + 1` to `end_line`.
```

**Notes:**
- Runs a fresh agent loop in the same workspace.
- Uses same config, tools, permissions as parent.
- Depth limited to 1 (no nested subagents).
- Useful for divide-and-conquer on complex tasks.

## Summary Table

| Tool                | Read | Write | Shell | Permission | Notes                          |
|---------------------|------|-------|-------|------------|--------------------------------|
| `read_file`         | Y    | —     | —     | always     | Line-numbered output           |
| `write_file`        | —    | Y     | —     | mode/tree  | Overwrites; destructive        |
| `edit_file`         | —    | Y     | —     | mode/tree  | Exact-match safe edit          |
| `apply_patch`       | —    | Y     | —     | mode/tree  | Atomic multi-file             |
| `glob_tool`         | Y    | —     | —     | always     | File discovery                 |
| `grep_tool`         | Y    | —     | —     | always     | Content search                 |
| `list_dir`          | Y    | —     | —     | always     | Directory listing              |
| `run_shell`         | —    | —     | Y     | mode/list  | Shell execution                |
| `web_fetch`         | Y    | —     | —     | always     | Download URL → text            |
| `web_search`        | Y    | —     | —     | always     | DuckDuckGo search (no key)     |
| `semantic_search`   | Y    | —     | —     | always     | RAG index search               |
| `find_symbol`       | Y    | —     | —     | always     | Where a symbol is defined      |
| `who_calls`         | Y    | —     | —     | always     | References + enclosing symbol  |
| `present_plan`      | —    | —     | —     | always     | Structured plan (gates in plan mode) |
| `save_memory`       | —    | Y     | —     | always     | Persist across sessions        |
| `recall_memory`     | Y    | —     | —     | always     | Fetch memory body              |
| `forget_memory`     | —    | Y     | —     | always     | Delete memory                  |
| `invoke_skill`      | Y    | —     | —     | always     | Load skill + track usage       |
| `recall_session`    | Y    | —     | —     | always     | Cross-project session search   |
| `read_session`      | Y    | —     | —     | always     | Fetch full session JSONL       |
| `spawn_subagent`    | —    | —     | —     | inherited  | Inner agent (depth=1)          |

## Error Handling

Most tools return `ToolOutcome { output: String, is_error: bool }`.

**Common error cases:**
- `read_file` on non-existent file → error, suggestion to `glob_tool` or `list_dir`.
- `edit_file` with no match → error, shows first 200 chars of file.
- `grep_tool` with invalid regex → error, invalid pattern reported.
- `run_shell` timeout → error, partial output before timeout.
- Permission denied → error, explains which mode allows it.

The agent can see errors and typically responds by adjusting the request or using a different tool.

## Performance Tips

1. **Use `glob_tool` before `read_file`** to find the right file.
2. **Truncation aware** — large files are head/tail truncated per `tool_output_max_lines`. Use `grep_tool` to find relevant sections first.
3. **Batch reads** — if you need multiple files, read them in sequence; async overhead is minimal.
4. **RAG first** — for code understanding, use `semantic_search` before grep; embeddings are faster than regex on large codebases.
5. **Shell commands** — cache output (don't re-run `cargo build` multiple times; save the result and reference it).
