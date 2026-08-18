# Configuration

Layered config, every knob, environment variables, and the permission model.
See [README](../README.md#configuration) for the short version.

`wingman` resolves configuration in this order (lowest to highest precedence):

1. Built-in defaults.
2. `~/.wingman/config.toml` (global).
3. `<project>/.wingman/config.toml` (project-local).
4. `WINGMAN_*` environment variables.
5. CLI flags.

TOML sub-tables are merged at the raw-TOML level, so an absent section in the
project file does **not** wipe out the global values for that section.

## Example `~/.wingman/config.toml`

```toml
default_provider = "anthropic"
permission_mode = "read-only"

[tokens]
compact_at_tokens = 120000
tool_output_max_lines = 400
prompt_cache = true
# max_usd_per_session = 5.0   # soft warning when estimated spend crosses this

[router]
fast_model = "anthropic/claude-haiku-4-5-20251001"
# local_model = "ollama/llama3.1"   # target of the `local` class keyword

[tui]
theme = "default"
show_token_usage = true

[tools]
# web_fetch/web_search are gated to auto-edit/yolo by default (network egress
# is an exfiltration channel). Set true to also allow them in read-only/plan.
allow_network = false
redact_output_secrets = true   # redact secret tokens in tool output (default on)

# Verification gate (runs after edits): compile check + affected tests + LSP
# diagnostics, and optional headless-browser visual check.
[verify]
turn_gate = "auto"        # "auto" | "off" | an explicit command
affected_tests = true
lsp_diagnostics = true
# [verify.browser]
# url = "http://localhost:5173"
# baseline = "tests/baseline.png"

# Aider-style: commit each turn's edits automatically.
[git]
auto_commit = false

# Append a compliance audit trail of every tool call.
[audit]
enabled = false

# Server-backed team memory (beyond the git-backed `memory sync`).
# [team]
# endpoint = "https://memory.example.com"
# token = "${WINGMAN_TEAM_TOKEN}"

# Extend the agent with your own shell-command tools (no recompile).
# [[tools.custom]]
# name = "run_migration"
# description = "Apply the latest DB migration"
# command = "make migrate"

[providers.anthropic]
api_key = "${ANTHROPIC_API_KEY}"
model = "claude-opus-4-7"

[providers.openai]
api_key = "${OPENAI_API_KEY}"
model = "gpt-4.1"

[providers.gemini]
api_key = "${GOOGLE_API_KEY}"
model = "gemini-2.5-pro"

[providers.openrouter]
api_key = "${OPENROUTER_API_KEY}"
model = "anthropic/claude-opus-4-7"

[providers.ollama]
base_url = "http://localhost:11434/v1"
model = "llama3.1:8b"

[providers.lmstudio]
base_url = "http://localhost:1234/v1"
model = "local-model"

[providers.vllm]
base_url = "http://localhost:8000/v1"
model = "local-model"

[providers.litellm]
api_key = "${LITELLM_API_KEY}"
base_url = "http://localhost:4000/v1"
model = "anthropic/claude-opus-4-7"

# MCP servers — each becomes a set of `mcp__<name>__<tool>` tools.
[mcp.filesystem]
transport = "stdio"                 # "stdio" (default) or "http"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
env = { API_KEY = "${SOME_KEY}" }   # env vars for the child process
# cwd = "/path/to/run/in"           # working directory for the child
# trusted = false                   # MCP tools are gated to auto-edit/yolo
                                    # unless a server is marked trusted (it
                                    # may run in read-only/plan mode then)

[mcp.remote]
transport = "http"                  # spec-compliant Streamable-HTTP client
url = "https://mcp.example.com/mcp" # (initialize handshake, SSE, session id)
headers = { Authorization = "Bearer ${MCP_TOKEN}" }  # auth / custom headers

[logging]
filter = "info,wingman=info"
file = true
```

## Environment variables

| Variable                            | Effect                                                              |
| ----------------------------------- | ------------------------------------------------------------------- |
| `WINGMAN_MODEL`                     | Overrides `default_model`. Same syntax as `--model`.                |
| `WINGMAN_PROVIDER`                  | Overrides `default_provider`.                                       |
| `WINGMAN_PERMISSION_MODE`           | `read-only` \| `auto-edit` \| `yolo`.                               |
| `WINGMAN_LOG`                       | `tracing-subscriber` env-filter directive.                          |
| `WINGMAN_<PROVIDER>_API_KEY`        | Sets `providers.<provider>.api_key`.                                |
| `WINGMAN_<PROVIDER>_BASE_URL`       | Sets `providers.<provider>.base_url`.                               |
| `WINGMAN_<PROVIDER>_MODEL`          | Sets `providers.<provider>.model`.                                  |

Any string field of the form `${ENV_VAR}` (e.g. `api_key = "${ANTHROPIC_API_KEY}"`)
is resolved against the environment at load time.

## Permission modes

| Mode         | Reads / Search | Writes inside project | Shell                        | Out-of-tree paths |
| ------------ | -------------- | --------------------- | ---------------------------- | ----------------- |
| `read-only`  | allowed        | denied                | denied                       | denied            |
| `plan`       | allowed        | after `/approve`      | after `/approve`             | denied            |
| `auto-edit`  | allowed        | auto-allowed          | auto-allowed except denylist | denied for writes |
| `yolo`       | allowed        | auto-allowed          | auto-allowed                 | allowed           |

**There are no interactive approval prompts.** A tool call that the active
mode doesn't permit is refused outright, with the reason returned to the
model. Choose the mode that matches the latitude you want to grant.

Enforcement is central: every tool declares what it needs
(`read` / `write` / `shell` / `network`), and the registry refuses the call
before the tool runs. A tool that declares nothing can only do pure
computation, so the default for anything new is deny.

**Protected paths.** `.git/`, `.wingman/config.toml`, `.wingman/skills/`, and
`.wingman/trusted.toml` are never writable — including in `yolo`. Each of
those turns a single bad edit into a change that outlives the session (a git
hook that fires on your next commit, a config that grants the agent more
permission next time). Edit them yourself if you mean to.

**About `plan`.** `plan` starts out identical to `read-only`: the agent can
read and search, but every write and shell call is refused. It calls
`present_plan` to show you what it intends to do; you run **`/approve`** in
the TUI to accept, and only then does it behave like `auto-edit` for the rest
of the session (still project-confined, and protected paths still refused).
Switching modes clears the approval, so returning to `plan` later needs a
fresh one — consent applies to the plan you actually read.

Headless (`--print`) has nobody to approve, so `plan` there stays read-only
for the whole run. Use `auto-edit` for unattended work.

**About `auto-edit`.** Writes are confined to the project tree. Shell is
confined too *when the platform provides a mechanism* — `bwrap` on Linux,
`sandbox-exec` on macOS — which bounds `run_shell` writes to the project and
blocks reads of `~/.ssh`, `~/.aws`, `~/.gnupg`. Set it with
`[tools].shell_sandbox`:

| value      | behaviour                                                        |
| ---------- | ---------------------------------------------------------------- |
| `auto`     | default — confine when available, otherwise run unconfined and warn |
| `required` | refuse to run shell at all when no mechanism is available          |
| `off`      | never wrap                                                        |

`wingman doctor` reports which mechanism is active, so it is a claim you can
check rather than take on trust. Two honest limits: there is **no Windows
mechanism wired up yet** (use `required` if that matters to you), and this
confines the filesystem, not the network — a sandboxed command can still
`curl`. The shell denylist remains a convenience, not a boundary.

`yolo` is per-session only — never persisted to config.

## Project layout on disk

```
.
├── Cargo.toml              # workspace manifest
├── Cargo.lock
├── rustfmt.toml
├── crates/
│   ├── wingman-cli/        # binary entry point
│   ├── wingman-config/     # config loading + merge
│   ├── wingman-core/       # provider-agnostic types + agent loop + LearningHook
│   ├── wingman-learn/      # memory, skill stats, session recall, hooks
│   ├── wingman-mcp/        # MCP host (M3)
│   ├── wingman-providers/  # Anthropic, ChatGPT, Gemini, OpenAI-compat
│   ├── wingman-rag/        # repo + session index (SQLite + fastembed/hash)
│   ├── wingman-session/    # JSONL session log + replay
│   ├── wingman-skills/     # markdown-frontmatter skills loader
│   ├── wingman-tools/      # built-in tools + registry
│   └── wingman-tui/        # ratatui surface
└── target/                 # build output (gitignored)
```

On the user's machine:

```
~/.wingman/
├── config.toml             # global config
├── credentials.toml        # provider credentials (optional)
├── logs/                   # tracing output
├── skills/                 # global skills (*.md)
├── memory/                 # global memories
│   ├── MEMORY.md           #   index — one bullet per memory
│   └── <slug>.md           #   per-memory body
├── learn.db                # skill usage + outcome stats (SQLite)
└── sessions.db             # cross-project session embeddings (SQLite)
```

```
<project-root>/.wingman/
├── config.toml             # project-local overrides
├── sessions/               # per-session JSONL logs (append-only)
├── index.db                # project RAG index (SQLite + embeddings)
├── skills/                 # project-scoped skills (override globals by name)
└── memory/                 # project-scoped memories
    ├── MEMORY.md
    └── <slug>.md
```
