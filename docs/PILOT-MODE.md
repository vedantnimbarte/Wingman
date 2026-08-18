# Pilot Mode

Multi-agent orchestration: plan a goal, run workers in isolated worktrees,
converge into a PR. Design notes live in [AUTONOMOUS-MODE.md](AUTONOMOUS-MODE.md).

`wingman pilot run "<goal>"` plans a multi-task piece of work, spawns
specialised worker agents in isolated git worktrees, and converges their
output into a single PR. The design record lives in
[AUTONOMOUS-MODE.md](AUTONOMOUS-MODE.md).

## Capability tiers

```
assist     You approve every decision. Agent plans, you confirm, agent executes
           one run, opens a PR, exits. No daemon, no critic, no learning.
copilot    Default. Agent flies; you monitor and intervene at decision points.
           Trust-tiered approval, self-healing retries, per-task reviewer,
           real verification, PR automation, cross-run learning.
autopilot  (experimental) Agent flies and navigates. Daemon mode, critic
           agent, knowledge graph, tool synthesis, sandboxed execution.
           Several autopilot capabilities are partial — see below.
```

> **Maturity.** `assist` and `copilot` are the supported tiers, and `copilot`
> now runs **end-to-end against a live provider** — it plans, spawns workers
> that write and commit code, reviews each task's real diff, squash-merges,
> and opens a PR. Validated on OpenRouter/DeepSeek (any tool-use-capable
> provider from the table below works). The per-task reviewer sends work back
> only on **high-severity** findings — a task's acceptance checks already gate
> functional correctness before it reaches review, so an over-eager reviewer
> model can't loop a correct change. The PR base branch is configurable via
> `[pilot.pr].base_branch` (default `main`).
> `autopilot` is experimental but most of its edges are now wired. The
> discovery daemon polls `github_issues`, `todos`, `ci_failures`,
> `dependabot`, `coverage_gaps` (reads an existing `lcov.info`), and
> `intake`. **Intake** is transport-agnostic: a Slack/email gateway writes
> `*.md` requests into `[pilot.daemon].intake_dir` and the
> daemon ingests them with per-author trust — no in-process listener needed.
> **Notification** delivery is wired via `[pilot.notifications.webhooks]`
> (channel → URL; Slack incoming-webhook shape; terminal fallback). **Mid-run
> steering** works — `pivot`/`clarify` IPC inject into the worker's next
> turn. **Auto-dispatch** (`[pilot.daemon].auto_dispatch`, off by default)
> opens real PRs autonomously; validate its trust config safely with
> `pilot daemon --dry-run` (logs what it *would* dispatch, opens nothing)
> before enabling it. Genuinely still open: the **`vm` sandbox tier** (real
> VM/Firecracker isolation — fail-closed today: pilot refuses vm-tier tasks
> rather than run them unsandboxed).

Pick a tier in `~/.wingman/config.toml`:

```toml
[pilot]
tier                  = "copilot"
default_model         = "anthropic/claude-opus-4-7"   # manager + reviewers
worker_model          = "anthropic/claude-haiku-4-5"  # workers
max_concurrent_agents = 4
max_usd               = 10.0
task_timeout_secs     = 1800
```

## Quick start

```bash
# One-shot: plan, approve, spawn workers, open PR
wingman pilot run "add a --version-only flag to wingman-cli"

# Plan only — write tasks.jsonl and exit
wingman pilot run --plan-only "<goal>"

# Auto-approve the plan (skip the y/e/n gate)
wingman pilot run --yes "<goal>"

# Dashboard
wingman pilot status              # one-shot summary of the latest run
wingman pilot watch               # live ASCII dashboard, polls state.json
wingman pilot watch <run-id>      # specific run

# Control a live run (via the control channel)
wingman pilot approve             # release a run waiting at the plan gate
wingman pilot veto                # reject a gated run
wingman pilot abort [--task <id>] # abort the whole run or one task
wingman pilot retry <task>        # retry a failed/blocked task
wingman pilot resume <run-id>     # resume an interrupted run
```

Per-run artefacts land under `<project>/.wingman/autonomous/<run-id>/`:

```
<run-id>/
  tasks.jsonl   # append-only event log
  state.json    # latest snapshot (rewritten after every event)
```

## Status

The full M1 pipeline is implemented (RunStore, planner, worker subprocess
with cross-platform supervisor, manager + orchestrator, git worktrees +
squash-merge, gh PR creation, dashboard, cost-cap enforcement, and the
provider-support gate). On top of that, the crate now ships the
`copilot`/`autopilot` machinery: a live control channel (`approve` /
`veto` / `abort` / `retry`), run `resume`, a per-run plan-approval gate,
sandbox tiers (`host` / `container` / `vm`, degrading to `host` when no
Docker daemon is present), and the always-on discovery `daemon` (five
sources: GitHub issues, TODOs, CI failures, Dependabot PRs, coverage gaps).
End-to-end `copilot` runs have been validated on a live provider
(OpenRouter/DeepSeek) — plan through PR; they need real API keys and are
**user-validated, not CI-validated** (CI runs the unit suite). Remaining
`autopilot`-only gaps: inbound Slack/email intake, the `vm` sandbox tier,
and live-validated auto-dispatch.

## Provider support for pilot mode

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

`wingman pilot run` prints a one-line support notice at startup and
refuses to start when the planner provider is `unsupported` (no current
backends are; the tier exists for future providers that can't emit
tool calls at all).
