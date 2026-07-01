# Arc-Code

[![ci](https://github.com/vedantnimbarte/ArcCode/actions/workflows/ci.yml/badge.svg)](https://github.com/vedantnimbarte/ArcCode/actions/workflows/ci.yml)

`arccode` is a multi-provider, terminal-first **self-improving** coding agent
written in Rust. It runs as a TUI for interactive sessions and as a headless
one-shot (`--print "prompt"`) for scripting, talks to 73+ LLM providers behind
a single streaming interface, ships a built-in tool layer for reading,
searching, and editing the project tree, and learns from every conversation:
it builds a persistent model of you and your projects, creates and refines
skills from observed work, and recalls past sessions across projects.

It is positioned as an open, provider-agnostic alternative to Claude Code,
Cursor, and Aider — with native support for Anthropic, OpenAI, ChatGPT
(OAuth), Google Gemini, OpenRouter, LiteLLM, LM Studio, vLLM, and Ollama,
a built-in MCP host that adapts external MCP-server tools as first-class
tools, and a multi-agent **pilot mode** that plans a goal, delegates to
worker agents in isolated worktrees, and converges into a PR.

---

## Quick Links

- **Getting Started:** See [Installation](#installation) and [Quick Start](#quick-start) below.
- **CLI Subcommands:** [CLI Reference](#cli-reference).
- **Documentation:** See [docs/](docs/) for detailed guides:
  - [docs/INDEX.md](docs/INDEX.md) — navigation guide for all technical docs.
  - [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — system design, crate overview, data flows.
  - [docs/TREE-SITTER.md](docs/TREE-SITTER.md) — language-aware parsing integration.
  - [docs/LEARNING-LOOP.md](docs/LEARNING-LOOP.md) — memories, skills, session recall.
  - [docs/TOOLS.md](docs/TOOLS.md) — complete reference for all 20+ built-in tools.
  - [docs/AUTONOMOUS-MODE.md](docs/AUTONOMOUS-MODE.md) — design doc for multi-task orchestration (now shipped as [Pilot mode](#pilot-mode)).
  - [docs/DIFFERENTIATION.md](docs/DIFFERENTIATION.md) — single-agent differentiation roadmap (routing, warm index, verification receipts, team memory).
- **For Developers:** See [Development](#development) below.

---

## Highlights

- **Self-improving learning loop.** Persistent memories (markdown +
  frontmatter under `~/.arccode/memory/` and `<project>/.arccode/memory/`),
  skill usage stats with outcome scoring, cross-session semantic recall via
  the existing RAG pipeline, and quiet-session nudges that ask the agent to
  consider persisting something when it's been a while since a save. See
  [Self-improving loop](#self-improving-loop) below.
- **73+ providers, one shape.** Anthropic is the reference implementation
  (streaming, tool use, explicit prompt caching). A single OpenAI-compatible
  adapter covers OpenAI, OpenRouter, LM Studio, vLLM, LiteLLM, and Ollama.
  Gemini and ChatGPT (OAuth) have their own adapters. All speak the same
  `arccode_core::Message` contract.
- **Three surfaces.** A `ratatui`-based TUI for interactive coding, a
  headless `--print` mode that emits either text or newline-delimited JSON
  events, and a `--batch <file.jsonl>` mode that runs a file of prompts
  non-interactively — all ready to pipe into other tools or CI.
- **MCP host.** Declare Model Context Protocol servers under `[mcp.<name>]`
  in config (stdio or HTTP transport); their tools are namespaced as
  `mcp__<server>__<tool>` and dispatched like built-ins. Manage them live
  from the TUI with `/mcp`.
- **Guided provider login.** `arccode login <provider>` (or `/login` in the
  TUI) probes the key, stores it in the OS keyring, and records the default
  model; `arccode logout <provider>` clears it. ChatGPT uses a browser
  OAuth flow.
- **Multi-agent pilot mode.** `arccode pilot run "<goal>"` plans, spawns
  worker agents in isolated worktrees, and opens a PR. See
  [Pilot mode](#pilot-mode).
- **`arccode knows`.** Prints what Arc-Code knows about the current project:
  memories, skills, model routing, the verification gate, and index
  freshness.
- **Built-in tool layer.** File read/write/edit, glob, grep, directory
  listing, shell execution, semantic search, and the new learning tools
  (`save_memory`, `recall_memory`, `invoke_skill`, `recall_session`,
  `read_session`), each gated by the active permission mode.
- **Live model swap.** Change provider/model mid-session with `/model
  <provider>/<id>` from inside the TUI — no restart, history preserved.
- **Token-aware pipeline.** Per-tool output budgets with head/tail
  truncation, history token estimation, and a compaction trigger
  (`compact_at_tokens`) so long sessions stay inside the active model's
  context window.
- **Layered configuration.** Defaults → global `~/.arccode/config.toml` →
  project `.arccode/config.toml` → `ARCCODE_*` env vars → CLI flags. TOML
  sub-tables merge instead of clobbering.
- **Permission modes.** `read-only` (default), `plan` (read-only + the
  agent must call `present_plan` before any edit), `auto-edit` (writes/shell
  inside the project tree auto-allowed, denylist still prompts), and `yolo`
  (no prompts; per-session only, never persisted).
- **Lifecycle hooks.** `pre_tool_use`, `post_tool_use`, `stop`, and
  `user_prompt_submit` shell hooks fire from the agent loop and can
  block a tool call by exiting non-zero (`[hooks]` in config).
- **Web tools.** Built-in `web_fetch` (URL → text) and `web_search`
  (DuckDuckGo HTML, no API key) tools pair for "look something up".
- **Atomic multi-file patches.** The `apply_patch` tool applies a
  multi-file edit block atomically — no partial writes on failure.
- **Working-tree checkpoints.** `arccode checkpoint` snapshots the tree
  into a tagged `git stash`; `arccode undo` restores the most recent one.
- **`arccode init`.** Scans the project (Cargo.toml, package.json,
  pyproject.toml, go.mod, …) and writes a starter `ARCCODE.md`.
- **`arccode cost`.** Per-model token + USD spend table derived from
  `~/.arccode/usage.json` and `pricing.rs`.
- **`arccode session list / fork`.** Browse recent session JSONLs;
  fork an old session (optionally truncating to N records) and resume it.
- **User-defined slash commands.** Drop a markdown file at
  `~/.arccode/commands/<name>.md` (or `<project>/.arccode/commands/`) and
  it becomes `/<name>` in the TUI. `$ARGS` is substituted.
- **In-transcript search.** `/find <query>`, `/findnext`, `/findprev`,
  `/findclear` walk hits inside the current transcript. Mouse wheel
  scrolling is enabled.
- **File-tree sidebar.** `Ctrl+B` toggles a left-side file browser; `j`/`k`
  move, `Tab` descends, `Enter` inserts the path into the composer.
- **Themes.** `tui.theme = "default" | "light" | "mono"`, plus optional
  per-role color overrides under `tui.colors` (`"#rrggbb"` hex or named).
- **Model fallback.** `router.fallback_models = ["openai/gpt-4.1",
  "openrouter/anthropic/claude-opus-4-7"]` — on primary failure the
  runtime walks the chain in order.
- **Subagent tool.** The model can call `spawn_subagent` to run an
  isolated inner agent loop on a focused sub-task (depth-capped at 1).
- **Notebook reads.** `read_file` on a `.ipynb` returns cells as fenced
  code blocks + markdown, not raw JSON.
- **Scheduled tasks.** `[[schedule]]` config entries fire from
  `arccode schedule` (call from cron / Task Scheduler).
- **Memory packs.** `arccode memory export/import/diff` for sharing
  team-level memory.
- **Worktree sandbox.** `arccode worktree create <branch>` spins up an
  isolated working copy under `.arccode/worktrees/`.
- **PR review.** `arccode review <pr#>` (or `--local <base>`) runs a
  one-shot review prompt against the diff.
- **Local model auto-discovery.** `arccode discover` probes localhost
  Ollama / LM Studio / vLLM and prints available models.
- **Skill auto-extraction.** `arccode skill extract` scans recent session
  JSONLs for repeated tool-call sequences (e.g. `grep_tool → read_file →
  edit_file`) and writes draft skill markdown files under
  `~/.arccode/skills/proposed/` for you to review.
- **Tree-sitter powered code understanding.** Deep language-aware parsing
  (Rust, Python, JavaScript, TypeScript, Go) for semantic chunking in the RAG
  index, symbol extraction, AST-aware diffs, and outline generation. Feature-gated
  so the workspace builds without the C toolchain if you don't need parsing.
- **Multi-model code review.** `arccode review-multi <pr#> --models
  anthropic/claude-opus-4-7,openai/gpt-4.1,gemini/gemini-2.5-pro` fans the
  review out across reviewers in parallel and merges findings by
  file:line, marking which ones each reviewer raised.
- **Interactive hunk review.** `arccode diff <file>` walks each hunk of
  the working-tree diff one at a time with `[a]ccept / [r]eject / [s]kip
  / [q]uit`, then writes the merged result. Also accepts `--patch
  <file.patch>` for an arbitrary unified diff.

---

## Workspace layout

This is a Cargo workspace. Each crate has a narrow, well-defined responsibility.

| Crate                | Role                                                                                                  |
| -------------------- | ----------------------------------------------------------------------------------------------------- |
| `arccode-cli`        | Binary entry point. Argument parsing, logging, runtime wiring, headless mode.                          |
| `arccode-core`       | Provider-agnostic types: `Message`, `ContentBlock`, `CompletionRequest`, `Provider`, agent loop, streaming events, tool dispatch, token estimation. |
| `arccode-config`     | TOML config loading, layered merge, env-var resolution, permission model.                              |
| `arccode-providers`  | Concrete `Provider` implementations: Anthropic, Gemini, ChatGPT, Cohere, Watsonx, OpenAI-compatible (68 variants). |
| `arccode-tools`      | Built-in tool implementations (`read_file`, `write_file`, `edit_file`, `glob`, `grep`, `list_dir`, `run_shell`) and the `ToolRegistry`. |
| `arccode-tui`        | `ratatui` interactive surface: composer, transcript, status bar, slash commands.                       |
| `arccode-session`    | Append-only JSONL session log + replay/reconstruction for `/resume`.                                   |
| `arccode-rag`        | SQLite-backed code index with `fastembed` (BGE small) or a deterministic hash embedder fallback.       |
| `arccode-skills`     | Markdown-frontmatter skill files (global + project), auto-loaded into the system prompt.               |
| `arccode-learn`      | Self-improving loop: persistent memory store, skill usage stats, session embedding/recall, agent hooks.|
| `arccode-mcp`        | MCP host: connects to stdio/HTTP MCP servers and adapts their tools as `arccode_core` tool dispatchers, namespaced `mcp__<server>__<tool>`. |
| `arccode-ts`         | Tree-sitter facade: language detection, symbol extraction, semantic chunking, syntax-aware diffs.    |
| `arccode-autonomous` | Pilot mode: multi-agent orchestrator that delegates a goal to worker agents in isolated worktrees and converges into a PR — planner, manager, control channel, sandbox tiers, discovery daemon. |

---

## Pilot mode

`arccode pilot run "<goal>"` plans a multi-task piece of work, spawns
specialised worker agents in isolated git worktrees, and converges their
output into a single PR. The full design lives in [`plan.md`](plan.md).

### Capability tiers

```
assist     You approve every decision. Agent plans, you confirm, agent executes
           one run, opens a PR, exits. No daemon, no critic, no learning.
copilot    Default. Agent flies; you monitor and intervene at decision points.
           Trust-tiered approval, self-healing retries, per-task reviewer,
           real verification, PR automation, cross-run learning.
autopilot  Agent flies and navigates. Daemon mode, multi-channel intake,
           critic agent, knowledge graph, tool synthesis, sandboxed execution.
```

Pick a tier in `~/.arccode/config.toml`:

```toml
[pilot]
tier                  = "copilot"
default_model         = "anthropic/claude-opus-4-7"   # manager + reviewers
worker_model          = "anthropic/claude-haiku-4-5"  # workers
max_concurrent_agents = 4
max_usd               = 10.0
task_timeout_secs     = 1800
```

### Quick start

```bash
# One-shot: plan, approve, spawn workers, open PR
arccode pilot run "add a --version-only flag to arccode-cli"

# Plan only — write tasks.jsonl and exit
arccode pilot run --plan-only "<goal>"

# Auto-approve the plan (skip the y/e/n gate)
arccode pilot run --yes "<goal>"

# Dashboard
arccode pilot status              # one-shot summary of the latest run
arccode pilot watch               # live ASCII dashboard, polls state.json
arccode pilot watch <run-id>      # specific run

# Control a live run (via the control channel)
arccode pilot approve             # release a run waiting at the plan gate
arccode pilot veto                # reject a gated run
arccode pilot abort [--task <id>] # abort the whole run or one task
arccode pilot retry <task>        # retry a failed/blocked task
arccode pilot resume <run-id>     # resume an interrupted run
```

Per-run artefacts land under `<project>/.arccode/autonomous/<run-id>/`:

```
<run-id>/
  tasks.jsonl   # append-only event log
  state.json    # latest snapshot (rewritten after every event)
```

### Status

The full M1 pipeline is implemented (RunStore, planner, worker subprocess
with cross-platform supervisor, manager + orchestrator, git worktrees +
squash-merge, gh PR creation, dashboard, cost-cap enforcement, and the
provider-support gate). On top of that, the crate now ships the
`copilot`/`autopilot` machinery: a live control channel (`approve` /
`veto` / `abort` / `retry`), run `resume`, a per-run plan-approval gate,
sandbox tiers (`host` / `container` / `vm`), and the always-on discovery
`daemon`. End-to-end runs against the providers below need real API keys
and are user-validated rather than CI-validated for now.

### Provider support for pilot mode

Pilot mode requires the model to emit structured tool-use blocks. The
table below classifies each backend; `untested` providers can still be
used, but quality depends on the local model's tool-use training.

| Provider     | Tier            | Notes                                                                  |
| ------------ | --------------- | ---------------------------------------------------------------------- |
| Anthropic    | `native`        | First-class tool use. Reference implementation.                        |
| Gemini       | `native`        | `functionCall` shape; first-class.                                     |
| OpenAI       | `openai-compat` | `tool_calls` shape; works on gpt-4o, gpt-4.1.                          |
| ChatGPT      | `openai-compat` | OAuth-backed; same shape as OpenAI.                                    |
| OpenRouter   | `openai-compat` | Aggregator — pass `provider/model` as model id.                        |
| LiteLLM      | `openai-compat` | Self-hosted gateway; works for any backend that LiteLLM speaks to.     |
| Groq         | `openai-compat` | Fast Llama/Mixtral hosting; native `tool_calls`.                       |
| Together     | `openai-compat` | OSS model catalog; tool-calls on Llama 3.1/3.3 + Qwen-Coder.           |
| Fireworks    | `openai-compat` | OSS + fine-tunes; documented tool-call support.                        |
| DeepInfra    | `openai-compat` | Cheap OSS hosting; OpenAI-shape.                                       |
| xAI (Grok)   | `openai-compat` | `grok-2` / `grok-2-vision`; supports `tool_calls`.                     |
| DeepSeek     | `openai-compat` | `deepseek-chat` / `deepseek-reasoner`.                                 |
| Mistral      | `openai-compat` | La Plateforme; codestral + mistral-large.                              |
| Cerebras     | `openai-compat` | Very fast Llama inference.                                             |
| SambaNova    | `openai-compat` | Llama 3.1 8B/70B/405B hosting.                                         |
| Azure OpenAI | `openai-compat` | Uses `api-key:` header; set `base_url` to your deployment.             |
| GitHub Models| `openai-compat` | Auth via `GITHUB_TOKEN`; rate-limited but free tier.                   |
| Perplexity   | `untested`      | Sonar models are search-augmented; tool use not guaranteed.            |
| LM Studio    | `untested`      | OpenAI-compat shim; depends on the loaded model.                       |
| vLLM         | `untested`      | Same: shape works, model has to be tool-trained.                       |
| Ollama       | `untested`      | Same: `/v1` shim, picks up whatever model you've pulled.               |
| llama.cpp    | `untested`      | `./server`'s `/v1` shim; depends on the loaded gguf.                   |
| HF TGI       | `untested`      | Text Generation Inference; OpenAI-compat endpoint on `:3000/v1`.       |
| AWS Bedrock  | `openai-compat` | Via Bedrock OpenAI surface + API key; Claude/Llama/Nova/Mistral.       |
| GCP Vertex AI| `openai-compat` | Via Vertex OpenAPI endpoint + `gcloud auth print-access-token`.        |
| IBM watsonx  | `native`        | Granite + hosted Llama; adapter handles IAM token exchange.            |
| Cohere       | `native`        | Command-R/A; native `/v2/chat` adapter with tool calls.                |
| Anyscale     | `openai-compat` | Endpoints hosting Llama 3.1/3.3 + Mixtral.                             |
| Lepton AI    | `openai-compat` | OSS + custom fine-tunes.                                               |
| Novita AI    | `openai-compat` | Cheap OSS hosting.                                                     |
| Hyperbolic   | `openai-compat` | Llama, DeepSeek, Qwen.                                                 |
| Lambda       | `openai-compat` | Lambda Labs Inference; Llama 3.1/3.3.                                  |
| Nebius       | `openai-compat` | Nebius AI Studio.                                                      |
| HF Inference | `openai-compat` | HuggingFace router; one HF token, many backends.                       |
| NVIDIA NIM   | `openai-compat` | `build.nvidia.com`; Llama-Nemotron, DeepSeek-R1.                       |
| Databricks   | `openai-compat` | Foundation Model APIs in your Databricks workspace.                    |
| Snowflake    | `openai-compat` | Cortex inference; set `base_url` to your account.                      |
| Replicate    | `untested`      | Via OpenAI proxy; tool support is model-dependent.                     |
| GLHF         | `untested`      | Long-tail HF model hosting.                                            |
| Featherless  | `untested`      | Long-tail HF model hosting.                                            |
| OctoAI       | `untested`      | Being deprecated; endpoint still works.                                |
| Avian        | `untested`      | Llama 3.1 hosting.                                                     |
| Kluster      | `untested`      | Llama hosting.                                                         |
| Inference.net| `untested`      | Batch + real-time OSS hosting.                                         |
| Writer       | `untested`      | Palmyra; tool-use varies by model.                                     |
| GPT4All      | `untested`      | Local REST server on `:4891/v1`.                                       |
| Jan / Cortex | `untested`      | Local on `:1337/v1`.                                                   |
| KoboldCpp    | `untested`      | Local OpenAI shim on `:5001/v1`.                                       |
| Oobabooga    | `untested`      | text-generation-webui OpenAI shim on `:5000/v1`.                       |

`arccode pilot run` prints a one-line support notice at startup and
refuses to start when the planner provider is `unsupported` (no current
backends are; the tier exists for future providers that can't emit
tool calls at all).

---

## Supported providers

| Provider           | id          | Env var                  | Default base URL                                  |
| ------------------ | ----------- | ------------------------ | ------------------------------------------------- |
| Anthropic          | `anthropic` | `ANTHROPIC_API_KEY`      | (native adapter)                                  |
| Google Gemini      | `gemini`    | `GOOGLE_API_KEY`         | (native adapter)                                  |
| ChatGPT (OAuth)    | `chatgpt`   | OAuth via `/login`       | (token in OS keychain)                            |
| OpenAI             | `openai`    | `OPENAI_API_KEY`         | `https://api.openai.com/v1`                       |
| OpenRouter         | `openrouter`| `OPENROUTER_API_KEY`     | `https://openrouter.ai/api/v1`                    |
| LiteLLM            | `litellm`   | `LITELLM_API_KEY`        | `http://localhost:4000/v1`                        |
| Groq               | `groq`      | `GROQ_API_KEY`           | `https://api.groq.com/openai/v1`                  |
| Together AI        | `together`  | `TOGETHER_API_KEY`       | `https://api.together.xyz/v1`                     |
| Fireworks AI       | `fireworks` | `FIREWORKS_API_KEY`      | `https://api.fireworks.ai/inference/v1`           |
| DeepInfra          | `deepinfra` | `DEEPINFRA_API_KEY`      | `https://api.deepinfra.com/v1/openai`             |
| Perplexity         | `perplexity`| `PERPLEXITY_API_KEY`     | `https://api.perplexity.ai`                       |
| xAI (Grok)         | `xai`       | `XAI_API_KEY`            | `https://api.x.ai/v1`                             |
| DeepSeek           | `deepseek`  | `DEEPSEEK_API_KEY`       | `https://api.deepseek.com/v1`                     |
| Mistral            | `mistral`   | `MISTRAL_API_KEY`        | `https://api.mistral.ai/v1`                       |
| Cerebras           | `cerebras`  | `CEREBRAS_API_KEY`       | `https://api.cerebras.ai/v1`                      |
| SambaNova          | `sambanova` | `SAMBANOVA_API_KEY`      | `https://api.sambanova.ai/v1`                     |
| Azure OpenAI       | `azure`     | `AZURE_OPENAI_API_KEY`   | (set to your deployment URL)                      |
| GitHub Models      | `github`    | `GITHUB_TOKEN`           | `https://models.inference.ai.azure.com`           |
| LM Studio          | `lmstudio`  | (none — local)           | `http://localhost:1234/v1`                        |
| vLLM               | `vllm`      | (none — local)           | `http://localhost:8000/v1`                        |
| Ollama             | `ollama`    | (none — local)           | `http://localhost:11434/v1`                       |
| llama.cpp server   | `llamacpp`  | (none — local)           | `http://localhost:8080/v1`                        |
| HF TGI             | `tgi`       | (none — local)           | `http://localhost:3000/v1`                        |
| Cohere             | `cohere`    | `COHERE_API_KEY`         | `https://api.cohere.com` (native `/v2/chat`)      |
| Anyscale           | `anyscale`  | `ANYSCALE_API_KEY`       | `https://api.endpoints.anyscale.com/v1`           |
| Lepton AI          | `lepton`    | `LEPTON_API_KEY`         | `https://api.lepton.ai/api/v1`                    |
| Replicate          | `replicate` | `REPLICATE_API_TOKEN`    | `https://openai-proxy.replicate.com/v1`           |
| Novita AI          | `novita`    | `NOVITA_API_KEY`         | `https://api.novita.ai/v3/openai`                 |
| Hyperbolic         | `hyperbolic`| `HYPERBOLIC_API_KEY`     | `https://api.hyperbolic.xyz/v1`                   |
| Lambda Inference   | `lambda`    | `LAMBDA_API_KEY`         | `https://api.lambdalabs.com/v1`                   |
| Nebius AI Studio   | `nebius`    | `NEBIUS_API_KEY`         | `https://api.studio.nebius.ai/v1`                 |
| HF Inference       | `hf`        | `HF_TOKEN`               | `https://router.huggingface.co/v1`                |
| GLHF.chat          | `glhf`      | `GLHF_API_KEY`           | `https://glhf.chat/api/openai/v1`                 |
| Featherless        | `featherless`| `FEATHERLESS_API_KEY`   | `https://api.featherless.ai/v1`                   |
| OctoAI             | `octoai`    | `OCTOAI_API_KEY`         | `https://text.octoai.run/v1`                      |
| NVIDIA NIM         | `nvidia`    | `NVIDIA_API_KEY`         | `https://integrate.api.nvidia.com/v1`             |
| Avian              | `avian`     | `AVIAN_API_KEY`          | `https://api.avian.io/v1`                         |
| Kluster.ai         | `kluster`   | `KLUSTER_API_KEY`        | `https://api.kluster.ai/v1`                       |
| Inference.net      | `inferencenet`| `INFERENCE_NET_API_KEY`| `https://api.inference.net/v1`                    |
| Snowflake Cortex   | `snowflake` | `SNOWFLAKE_API_KEY`      | (set to your account URL)                         |
| Databricks         | `databricks`| `DATABRICKS_TOKEN`       | (set to your workspace URL)                       |
| Writer Palmyra     | `writer`    | `WRITER_API_KEY`         | `https://api.writer.com/v1`                       |
| GPT4All            | `gpt4all`   | (none — local)           | `http://localhost:4891/v1`                        |
| Jan / Cortex       | `jan`       | (none — local)           | `http://localhost:1337/v1`                        |
| KoboldCpp          | `koboldcpp` | (none — local)           | `http://localhost:5001/v1`                        |
| Oobabooga          | `oobabooga` | (none — local)           | `http://localhost:5000/v1`                        |
| Alibaba Qwen       | `qwen`      | `DASHSCOPE_API_KEY`      | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` |
| Zhipu GLM          | `zhipu`     | `ZHIPU_API_KEY`          | `https://open.bigmodel.cn/api/paas/v4`            |
| Moonshot Kimi      | `moonshot`  | `MOONSHOT_API_KEY`       | `https://api.moonshot.cn/v1`                      |
| MiniMax            | `minimax`   | `MINIMAX_API_KEY`        | `https://api.minimaxi.com/v1`                     |
| Yi (01.AI)         | `yi`        | `YI_API_KEY`             | `https://api.lingyiwanwu.com/v1`                  |
| Baichuan           | `baichuan`  | `BAICHUAN_API_KEY`       | `https://api.baichuan-ai.com/v1`                  |
| Tencent Hunyuan    | `hunyuan`   | `HUNYUAN_API_KEY`        | `https://api.hunyuan.cloud.tencent.com/v1`        |
| ByteDance Doubao   | `doubao`    | `ARK_API_KEY`            | `https://ark.cn-beijing.volces.com/api/v3`        |
| SiliconFlow        | `siliconflow`| `SILICONFLOW_API_KEY`   | `https://api.siliconflow.cn/v1`                   |
| Cloudflare Workers | `cloudflare`| `CLOUDFLARE_API_TOKEN`   | (set to your account-id URL)                      |
| Vercel AI Gateway  | `vercel`    | `VERCEL_AI_GATEWAY_KEY`  | `https://gateway.ai.vercel.com/v1`                |
| AIMLAPI            | `aimlapi`   | `AIMLAPI_KEY`            | `https://api.aimlapi.com/v1`                      |
| OpenPipe           | `openpipe`  | `OPENPIPE_API_KEY`       | `https://api.openpipe.ai/api/v1`                  |
| Targon             | `targon`    | `TARGON_API_KEY`         | `https://api.targon.com/v1`                       |
| Pollinations       | `pollinations`| (none — free tier)     | `https://text.pollinations.ai/openai/v1`          |
| AI21 Jamba         | `ai21`      | `AI21_API_KEY`           | `https://api.ai21.com/studio/v1`                  |
| Z.ai (GLM coding)  | `zai`       | `ZAI_API_KEY`            | `https://api.z.ai/api/coding/paas/v4`             |
| Friendli AI        | `friendli`  | `FRIENDLI_TOKEN`         | `https://inference.friendli.ai/v1`                |
| Mancer             | `mancer`    | `MANCER_API_KEY`         | `https://neuro.mancer.tech/oai/v1`                |
| Reka               | `reka`      | `REKA_API_KEY`           | `https://api.reka.ai/v1`                          |
| mlx-lm-server      | `mlx`       | (none — local)           | `http://localhost:8080/v1`                        |
| LocalAI            | `localai`   | (none — local)           | `http://localhost:8080/v1`                        |
| Aphrodite Engine   | `aphrodite` | (none — local)           | `http://localhost:2242/v1`                        |
| Mistral.rs server  | `mistralrs` | (none — local)           | `http://localhost:1234/v1`                        |
| AWS Bedrock        | `bedrock`   | `AWS_BEARER_TOKEN_BEDROCK`| `https://bedrock-runtime.<region>.amazonaws.com/openai/v1` |
| GCP Vertex AI      | `vertex`    | `GOOGLE_VERTEX_TOKEN`    | (set to your project/region OpenAPI URL)          |
| IBM watsonx.ai     | `watsonx`   | `WATSONX_API_KEY` + `WATSONX_PROJECT_ID` | `https://<region>.ml.cloud.ibm.com` |

All non-Anthropic / non-Gemini / non-ChatGPT / non-Cohere entries share the
`OpenAiCompatProvider` adapter (`crates/arccode-providers/src/openai_compat.rs`).
Add a new hosted OpenAI-shape clone by extending its `Variant` enum and the
mapper functions in `runtime.rs` + `login.rs`.

**Notes on the enterprise providers (Bedrock / Vertex / watsonx):**

- **AWS Bedrock** ships via the OpenAI-compat surface released in 2024 —
  set `AWS_BEARER_TOKEN_BEDROCK` (long-term API key generated from the
  AWS console) and adjust the region in `base_url`. The SigV4 path
  against `/model/<id>/invoke-with-response-stream` (with the AWS Event
  Stream binary framing) is **not** implemented; if your AWS setup
  doesn't permit Bedrock API keys, that adapter is the follow-up work.
- **GCP Vertex AI** uses the OpenAPI endpoint with an OAuth2 access
  token. Populate `GOOGLE_VERTEX_TOKEN` with the output of
  `gcloud auth print-access-token` (refresh hourly) and set `base_url`
  to your project + region. Service-account JWT signing is the
  follow-up work for unattended use.
- **IBM watsonx.ai** is a native adapter (`watsonx.rs`) — provide
  `WATSONX_API_KEY` + `WATSONX_PROJECT_ID` and the adapter exchanges
  the API key for an IAM token internally (cached for ~1h). Pass
  `WATSONX_ACCESS_TOKEN` instead if you've already minted one.

---

## Installation

### Prerequisites

- Rust 1.80 or later (uses 2021 edition; pinned in `Cargo.toml`).
- A working C toolchain for some transitive crates.
- (Optional) An API key for the provider(s) you intend to use.

### Build from source

```bash
git clone git@github.com:vedantnimbarte/Arc-Code.git
cd Arc-Code
cargo build --release
```

The resulting binary is at `target/release/arccode` (or `arccode.exe` on
Windows).

To install onto your `PATH`:

```bash
cargo install --path crates/arccode-cli
```

---

## Quick start

### 1. Scaffold a config

```bash
arccode config init
```

This writes a starter `~/.arccode/config.toml` populated with entries for every
supported provider, each pointing at a `${ENV_VAR}` placeholder for the API
key.

### 2. Set an API key

Pick one of the supported providers and export its key. Anthropic is the
default:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
# or
export OPENAI_API_KEY=sk-...
export OPENROUTER_API_KEY=...
export GOOGLE_API_KEY=...
```

For local providers (Ollama, LM Studio, vLLM) no key is needed — just point
the `base_url` at the running instance.

### 3. Use it

```bash
# Interactive TUI in the current project
arccode

# Headless one-shot
arccode --print "explain the agent loop in crates/arccode-core"

# Headless, streaming JSON events (newline-delimited)
arccode --print "list the public types in arccode-core" --json

# Pick a model for this session only
arccode --model anthropic/claude-opus-4-7
arccode --model openai/gpt-4.1
arccode --model gemini/gemini-2.5-pro
arccode --model openrouter/anthropic/claude-opus-4-7

# Loosen the permission model for this session
arccode --mode auto-edit
arccode --mode yolo            # no prompts; per-session only
```

### Inside the TUI

- Type a prompt and hit Enter to send. A `/`-prefixed line shows a slash
  autocomplete popup.
- `/model [<provider>/<model-id>]` — swap the active model live; empty arg
  opens the picker.
- `/mode <read-only|auto-edit|yolo>` — change the permission mode live.
- `/login` (`/connect`) — guided provider-connect wizard; `/logout [name]`.
- `/mcp` — add / remove / connect / disconnect MCP servers.
- `/memory` — list saved memories. `/memory forget <name>` to delete one.
- `/recall <query>` — search across past sessions for prior context.
- `/skills [new <name>]` — browse and apply skills, or scaffold a new one;
  `/skill <name>` queues a skill and `/skill stats [name]` shows usage counts.
- `/learn [status|reset]` — self-learning loop dashboard.
- `/usage` — per-model token + cost breakdown. `/params` — model params.
- `/add <path>` — attach a file to the next prompt.
- `/export [md]` — write the transcript to a file. `/resume` — reload the
  last session.
- `/find <query>` — search the transcript (`/findnext`, `/findprev`,
  `/findclear`).
- Tool calls render inline with their output (head/tail truncated per the
  active budget) and the token-usage strip updates after each turn.

Tell the agent things like "remember that I prefer pnpm over npm" or "from
now on always run `cargo fmt` before commits" — it will call `save_memory`
and the next session will see it in the system prompt.

---

## Self-improving loop

Every session contributes to a small set of files under `~/.arccode/` and
`<project>/.arccode/` that subsequent runs read on startup. There is no
cloud component — everything is local-first.

### What's persisted

- **Memories** at `~/.arccode/memory/<slug>.md` (global) or
  `<project>/.arccode/memory/<slug>.md` (project), indexed by a sibling
  `MEMORY.md`. Each memory is markdown with YAML frontmatter
  (`name`, `description`, `type`). Four types:

  | Type        | Default scope | Used for                                                       |
  | ----------- | ------------- | -------------------------------------------------------------- |
  | `user`      | global        | Facts about the human (role, expertise, working style).        |
  | `feedback`  | global        | How to behave (terse responses, avoid mocks, etc.).            |
  | `project`   | project       | Facts about this codebase (build commands, conventions).       |
  | `reference` | global        | Pointers to external systems (issue tracker, dashboards).      |

  The memory **index** (one bullet per memory) is rendered into the system
  prompt every turn. Full bodies stay on disk; the agent fetches them via
  `recall_memory` when relevant.

- **Skill usage** at `~/.arccode/learn.db` (SQLite). Every `invoke_skill`
  call is recorded; the next user turn flips its outcome to `success` or
  `corrected` based on negation heuristics ("no,", "wait,", "wrong,",
  "actually,"…). When a skill crosses 3 invocations with ≥50% correction
  rate, the next session's system prompt suggests a rewrite.

- **Session embeddings** at `~/.arccode/sessions.db`. Finished session
  JSONLs are chunked into thread-shaped windows and embedded using the
  same `fastembed`/hash backend that powers `semantic_search`. The CLI
  backfills any unindexed sessions in the background at startup.

### Learning tools (callable by the agent)

| Tool             | When the agent uses it                                                          |
| ---------------- | ------------------------------------------------------------------------------- |
| `save_memory`    | User says "remember", "from now on", expresses a stable preference.             |
| `recall_memory`  | The memory index in the prompt hints at relevance and the agent needs the body.|
| `forget_memory`  | User explicitly asks to forget, or a memory is clearly wrong.                   |
| `invoke_skill`   | A skill from the catalog matches the task; instructions apply for the turn.    |
| `recall_session` | "Have we discussed X before?" / "How did we fix Y last time?"                   |
| `read_session`   | Drill into a specific session id returned by `recall_session`.                  |

### Nudges

After a configurable number of quiet sessions (default 5 — no saves), the
system prompt for the next turn includes a one-line nudge asking the agent
to consider proposing a memory if anything surprising came up. `/learn
reset` zeros the counter; `/learn status` shows where you stand.

---

## Hooks

User-defined shell hooks fire at four well-known points. Configure under
`[hooks]` in `config.toml`:

```toml
[[hooks.pre_tool_use]]
command = "cargo fmt --check"
match_tool = "edit_file"      # also matches "edit_file*" or "*"
block = true                  # exit != 0 cancels the tool call
timeout_secs = 10

[[hooks.post_tool_use]]
command = "echo \"$ARCCODE_TOOL_NAME ran\""

[[hooks.stop]]
command = "notify-send 'arccode done'"

[[hooks.user_prompt_submit]]
command = "grep -qiv secret <<< \"$ARCCODE_USER_PROMPT\""
block = true                  # reject prompts containing 'secret'
```

The agent loop populates per-event environment variables
(`ARCCODE_TOOL_NAME`, `ARCCODE_TOOL_INPUT`, `ARCCODE_TOOL_OUTPUT`,
`ARCCODE_TOOL_IS_ERROR`, `ARCCODE_STOP_REASON`, `ARCCODE_USER_PROMPT`).
Hooks run via `sh -c` on Unix and `cmd /C` on Windows, with the
configured `timeout_secs` (default 10).

---

## User-defined slash commands

Place markdown files at `~/.arccode/commands/<name>.md` (global) or
`<project>/.arccode/commands/<name>.md` (project). When the user types
`/<name> rest of line` in the TUI, the markdown body is expanded into the
prompt with the literal token `$ARGS` replaced by `rest of line`, and
submitted as if typed directly. Project-local commands shadow globals.

Example `~/.arccode/commands/refactor.md`:

```markdown
Refactor the following Rust code with these constraints:
1. Keep the public API unchanged.
2. Prefer iterators over explicit loops.
3. Run `cargo clippy` mentally and address obvious lints.

$ARGS
```

Then in the TUI: `/refactor crates/foo/src/lib.rs` expands to a complete prompt.

---

## Configuration

`arccode` resolves configuration in this order (lowest to highest precedence):

1. Built-in defaults.
2. `~/.arccode/config.toml` (global).
3. `<project>/.arccode/config.toml` (project-local).
4. `ARCCODE_*` environment variables.
5. CLI flags.

TOML sub-tables are merged at the raw-TOML level, so an absent section in the
project file does **not** wipe out the global values for that section.

### Example `~/.arccode/config.toml`

```toml
default_provider = "anthropic"
permission_mode = "read-only"

[tokens]
compact_at_tokens = 120000
tool_output_max_lines = 400
prompt_cache = true

[router]
fast_model = "anthropic/claude-haiku-4-5-20251001"

[tui]
theme = "default"
show_token_usage = true

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

[mcp.remote]
transport = "http"
url = "http://localhost:9000/mcp"

[logging]
filter = "info,arccode=info"
file = true
```

### Environment variables

| Variable                            | Effect                                                              |
| ----------------------------------- | ------------------------------------------------------------------- |
| `ARCCODE_MODEL`                     | Overrides `default_model`. Same syntax as `--model`.                |
| `ARCCODE_PROVIDER`                  | Overrides `default_provider`.                                       |
| `ARCCODE_PERMISSION_MODE`           | `read-only` \| `auto-edit` \| `yolo`.                               |
| `ARCCODE_LOG`                       | `tracing-subscriber` env-filter directive.                          |
| `ARCCODE_<PROVIDER>_API_KEY`        | Sets `providers.<provider>.api_key`.                                |
| `ARCCODE_<PROVIDER>_BASE_URL`       | Sets `providers.<provider>.base_url`.                               |
| `ARCCODE_<PROVIDER>_MODEL`          | Sets `providers.<provider>.model`.                                  |

Any string field of the form `${ENV_VAR}` (e.g. `api_key = "${ANTHROPIC_API_KEY}"`)
is resolved against the environment at load time.

### Permission modes

| Mode         | Reads / Search | Writes inside project | Shell                       | Out-of-tree paths |
| ------------ | -------------- | --------------------- | --------------------------- | ----------------- |
| `read-only`  | allowed        | prompts               | prompts                     | prompts           |
| `plan`       | allowed        | denied (plan first)   | denied                      | denied            |
| `auto-edit`  | allowed        | auto-allowed          | auto-allowed except denylist | prompts           |
| `yolo`       | allowed        | auto-allowed          | auto-allowed                | auto-allowed      |

In `plan` mode the assistant is expected to call `present_plan` and wait for
the user before requesting any write/shell tool. The `present_plan` tool is
always available so the model can produce a structured plan even outside
plan mode (it just won't gate anything in that case).

`yolo` is per-session only — never persisted to config.

---

## CLI reference

```text
arccode [OPTIONS] [COMMAND]
```

**Top-level flags**

| Flag                     | Description                                                                 |
| ------------------------ | --------------------------------------------------------------------------- |
| `--mode <MODE>`          | `read-only` \| `auto-edit` \| `yolo`.                                       |
| `--model <MODEL>`        | Model id, optionally prefixed: `anthropic/claude-opus-4-7`. Env: `ARCCODE_MODEL`. |
| `--print <PROMPT>`       | Run a single prompt and exit (non-interactive).                              |
| `--batch <FILE>`         | Run a JSONL file of prompts non-interactively. Pairs with `--json`.          |
| `--json`                 | Emit newline-delimited JSON events instead of text. Use with `--print`/`--batch`. |
| `-v`, `-vv`              | Increase log verbosity.                                                      |
| `--quiet`                | Suppress non-error stderr output.                                            |
| `--version`              | Print version and exit.                                                      |
| `--help`                 | Print help.                                                                  |

**Subcommands**

| Command              | Description                                            |
| -------------------- | ------------------------------------------------------ |
| `config init`        | Write a starter `~/.arccode/config.toml`. `--force` to overwrite. |
| `config show`        | Print the merged effective configuration. `--json` for JSON output. |
| `config paths`       | Print the resolved global and project config paths.    |
| `login [provider]`   | Probe a provider key, store it in the OS keyring, record the default model. `--list` shows provider ids; `--oauth` forces the ChatGPT browser flow; `--no-probe` / `--no-default` / `--base-url` / `--model` refine it. |
| `logout <provider>`  | Delete a provider's stored credential from the OS keyring. |
| `knows`              | Show what Arc-Code knows about this project: memories, skills, model routing, the verification gate, and index freshness. |
| `init`               | Scan the current project and write a starter `ARCCODE.md`. `--force` to overwrite. |
| `checkpoint`         | Snapshot the working tree into a tagged `git stash`. `--label <text>` for a note. |
| `undo`               | Restore the most recent `arccode checkpoint` via `git stash pop`. |
| `cost`               | Show per-model token usage and estimated USD spend. `--json` for JSON. |
| `session list`       | List recent session JSONL files for this project.       |
| `session fork`       | Copy an existing session into a new file (`--at N` truncates). |
| `worktree create <branch>` | Create a `git worktree` under `.arccode/worktrees/<branch>` for sandboxed experiments. |
| `worktree list`      | `git worktree list` passthrough.                        |
| `worktree remove <path>` | Remove a worktree by path.                          |
| `memory export <out>` | Export the global memory dir to a directory or `.json` pack. |
| `memory import <path>` | Import a memory pack (`--force` to overwrite).        |
| `memory diff <a> <b>` | Show differences between two packs (or live dir vs. pack). |
| `review <pr#>`       | Fetch a PR diff via `gh` and run a one-shot review prompt. `--local <base>` for git-local diff. `--template <file>` for a custom prompt. |
| `discover`           | Probe localhost for Ollama / LM Studio / vLLM and list their models. |
| `schedule [--all]`   | Run any `[[schedule]]` entries whose cadence is due (cron-callable). |
| `skill extract`      | Mine recent session JSONLs for repeated tool-call sequences and write proposed skill drafts under `~/.arccode/skills/proposed/`. `--min N` (default 2), `--force` to overwrite. |
| `review-multi`       | Run a code-review prompt across multiple `provider/model` reviewers in parallel and merge findings by file:line. `--models a,b,c`. |
| `diff <file>` / `diff --patch <p>` | Interactive hunk-by-hunk accept/reject reviewer that writes the merged result back to the working tree. |
| `pilot run "<goal>"` | Plan a goal, spawn worker agents in isolated worktrees, open a PR. Flags: `--plan-only`, `--yes`, `--review`, `--watch`, `--no-pr`, `--base <rev>`, `--max-agents <n>`, `--max-usd <f>`, `--sandbox <host\|container\|vm>`, `--await-approval`. |
| `pilot status [run-id]` | One-shot ASCII summary of a run.                  |
| `pilot watch [run-id]` | Live dashboard that redraws on `state.json` changes. |
| `pilot resume <run-id>` | Resume an interrupted run; re-queues stuck tasks. |
| `pilot daemon`       | Always-on discovery daemon (requires `[pilot.daemon] enabled`). |
| `pilot abort` / `pilot retry <task>` | Control a live run via its control channel. |
| `pilot approve` / `pilot veto` | Approve or reject a run waiting at the plan-approval gate. |

Running `arccode` with no subcommand launches the TUI against the resolved
provider and model.

> `arccode autonomous "<goal>"` is a deprecated alias for `arccode pilot
> run` — kept through M3, removed at M4.

---

## Built-in tools

Each tool runs through the registry, which receives a `ToolCtx` carrying the
active permission mode, current working directory, and project root. Tools
decide whether to act, prompt, or refuse based on that context.

| Tool              | Purpose                                                                 |
| ----------------- | ----------------------------------------------------------------------- |
| `read_file`       | Read a file by absolute path. Returns content with line numbers.        |
| `write_file`      | Create or overwrite a file.                                             |
| `edit_file`       | Exact string replacement inside an existing file.                       |
| `apply_patch`     | Multi-file atomic edit (Update / Add / Delete blocks).                  |
| `spawn_subagent`  | Run an isolated inner agent loop on a sub-task; depth-capped at 1.      |
| `glob_tool`       | Find files by glob pattern (e.g. `**/*.rs`).                            |
| `grep_tool`       | Content search via ripgrep semantics.                                   |
| `list_dir`        | List a directory.                                                       |
| `run_shell`       | Execute a shell command. Subject to the permission denylist.            |
| `web_fetch`       | Download an http(s) URL, strip HTML, return text.                       |
| `web_search`      | DuckDuckGo HTML search (no API key); pairs with `web_fetch`.            |
| `semantic_search` | Cosine search the project RAG index for relevant code chunks.           |
| `present_plan`    | Structured plan block; required step before edits in `plan` mode.       |
| `save_memory`     | Persist a fact / preference / instruction across sessions.              |
| `recall_memory`   | Read the full body of a memory by slug.                                 |
| `forget_memory`   | Delete a memory by slug.                                                |
| `invoke_skill`    | Load a named skill's body for the current turn; records into stats.     |
| `recall_session`  | Cross-project semantic search over past session transcripts.            |
| `read_session`    | Fetch a full session JSONL by id.                                       |

Tool output is bounded by `tokens.tool_output_max_lines`; anything longer is
head/tail truncated before being fed back into the model.

---

## Roadmap

The project is being built milestone by milestone:

- **M0** — Workspace scaffold, CLI surface, layered config loader. *(shipped)*
- **M1** — Headless and TUI agent loop against Anthropic with built-in tools. *(shipped)*
- **M2** — Six more providers, token pipeline, live `/model` swap. *(shipped)*
- **M3** — Session persistence (`arccode-session`), `/resume`, MCP host
  (`arccode-mcp`): stdio/HTTP transports, `[mcp]` config, `/mcp` management,
  tools namespaced as `mcp__<server>__<tool>`. *(shipped)*
- **M4** — Repo index / RAG (`arccode-rag`) with SQLite store and `fastembed`
  or hash-embedder fallback. *(shipped)*
- **M5** — Skills (`arccode-skills`), ChatGPT OAuth, TUI polish (welcome
  screen, slash autocomplete). *(shipped)*
- **M6** — Self-improving learning loop (`arccode-learn`): persistent
  memories, skill usage stats with outcome scoring, cross-session recall,
  nudges. *(shipped)*
- **M7** — Tree-sitter integration across RAG, tools, diff/review, TUI. *(shipped)*
- **Pilot mode** — Multi-agent orchestration (`arccode pilot`): multi-task
  planning, worker agents in isolated worktrees, squash-merge + PR, capability
  tiers, control channel, resume, sandbox tiers, discovery daemon. *(shipped;
  end-to-end runs are user-validated)* `arccode autonomous` is a deprecated
  alias.
- **Next** — Interactive TUI approval modal for skill/memory proposals,
  session logging from the TUI (currently headless-only), autopilot-tier
  hardening (critic, knowledge graph, tool synthesis).

---

## Development

### Build & test

```bash
cargo build              # debug build
cargo build --release    # release build
cargo test               # full test suite
cargo fmt                # formatting (rustfmt.toml is project-pinned)
cargo clippy             # lints
```

### Run the TUI from source

```bash
cargo run -- --mode auto-edit
```

### Run a headless one-shot from source

```bash
cargo run -- --print "what does crates/arccode-core do?"
```

### Logs

By default, logs are written to `~/.arccode/logs/`. Override with
`ARCCODE_LOG=debug` or via the `[logging]` block in config.

---

## Project layout on disk

```
.
├── Cargo.toml              # workspace manifest
├── Cargo.lock
├── rustfmt.toml
├── crates/
│   ├── arccode-cli/        # binary entry point
│   ├── arccode-config/     # config loading + merge
│   ├── arccode-core/       # provider-agnostic types + agent loop + LearningHook
│   ├── arccode-learn/      # memory, skill stats, session recall, hooks
│   ├── arccode-mcp/        # MCP host (M3)
│   ├── arccode-providers/  # Anthropic, ChatGPT, Gemini, OpenAI-compat
│   ├── arccode-rag/        # repo + session index (SQLite + fastembed/hash)
│   ├── arccode-session/    # JSONL session log + replay
│   ├── arccode-skills/     # markdown-frontmatter skills loader
│   ├── arccode-tools/      # built-in tools + registry
│   └── arccode-tui/        # ratatui surface
└── target/                 # build output (gitignored)
```

On the user's machine:

```
~/.arccode/
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
<project-root>/.arccode/
├── config.toml             # project-local overrides
├── sessions/               # per-session JSONL logs (append-only)
├── index.db                # project RAG index (SQLite + embeddings)
├── skills/                 # project-scoped skills (override globals by name)
└── memory/                 # project-scoped memories
    ├── MEMORY.md
    └── <slug>.md
```

---

## License

Dual-licensed under either:

- MIT License
- Apache License, Version 2.0

at your option.

---

## Contributing

Issues and pull requests are welcome. Before opening a PR:

1. `cargo fmt` and `cargo clippy` cleanly.
2. `cargo test` passes.
3. New behavior is covered by a test where reasonable.
