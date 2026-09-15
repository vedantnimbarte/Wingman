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
> (channel → URL; Slack incoming-webhook shape; terminal fallback), and the
> `desktop` channel is real once `[pilot.notifications].desktop_inbox = true`:
> it writes cards the `wingman notify` popup renders, with Approve/Veto on the
> plan gate. Off by default. Note `progress` routes to `digest`, so turning it
> on surfaces failures and gates but not successful completions — set
> `progress = "desktop"` for those. See [NOTIFIER.md](NOTIFIER.md). **Mid-run
> steering** works — `pilot tell` / `pilot ask` (and the `pivot`/`clarify` IPC
> underneath) inject into the worker's next turn, and `ask` waits for the
> worker's reply. **Auto-dispatch** (`[pilot.daemon].auto_dispatch`, off by default)
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
worker_max_turns      = 60                            # model turns per worker
max_manager_ticks     = 200                           # scheduling decisions per run
```

`worker_max_turns` is deliberately far above the interactive default. A chat
turn answers a question; a worker has to read the code, edit it, run a build,
read the errors, fix them, re-run, and only then report. Running out of turns
is the one failure that looks like success from the outside — the process
exits 0 having quietly abandoned the job — so the budget is generous and
`task_timeout_secs` plus `max_usd` are the real ceilings, both of which fail
loudly.

`max_manager_ticks` bounds manager *activity*, not elapsed time: ticks where
work is in flight and nothing needs deciding are free. Before that was true,
one slow task could exhaust the budget just by being watched, and the run
died while its worker was still making progress.

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

## Worker transcripts

Each worker writes its own session log under
`<project>/.wingman/sessions/<session-id>.jsonl` — the id the orchestrator
minted and reports on the board, so a worker's turns can be inspected,
forked, or resumed like any other session:

```bash
wingman session fork <session-id>
```

The log goes to the **owning project**, not the worker's worktree
(decision record: [0006](decisions/0006-two-project-roots.md)). Workers run
in `<project>/.wingman/worktrees/<name>`, which git marks with a `.git` *file*
— so ordinary project-root discovery stops there, and a transcript written
under it would be force-removed with the worktree at cleanup. Worker
transcripts are also queued for the recall index, so `recall_session` can find
what a past worker did.

## Acceptance checks

Every task carries executable checks the worker must pass before review:
`shell` (exit 0), `grep` (literal or regex match in a file), `run` (execute
the app), `assert` (a rendered artifact contains text), and `http`:

```jsonc
{"kind": "http", "url": "http://localhost:3000/api/version",
 "must_match": 200,                      // status code, body substring, or omitted (< 400)
 "schema": {                             // optional: body must be JSON matching this
   "type": "object",
   "required": ["version"],
   "properties": {"version": {"type": "string", "pattern": "^\\d+\\.\\d+"}}
 }}
```

`schema` supports `type`, `enum`, `const`, `properties`, `required`,
`additionalProperties`, `items`, `minItems`/`maxItems`,
`minLength`/`maxLength`, `pattern`, `minimum`/`maximum` (and the exclusive
forms), and `allOf`/`anyOf`/`oneOf`/`not`; `title`, `description`, `format`
and the other annotations are ignored. Any other keyword (`$ref`,
`patternProperties`, `if`, ...) fails the check with "unsupported schema
keyword" rather than being skipped, so a schema that cannot be fully checked
never passes.

## Security pass

Before the auto-merge gate, every run that opens a PR gets a security pass
over the integration branch. Its summary is posted on the PR as a comment
(when `gh` opened it) and printed by `wingman pilot run`; any finding at or
above `block_severity` blocks auto-merge.

- **Secrets.** A built-in scan (known key prefixes plus entropy) over the
  added lines always runs. When `gitleaks` is on PATH it also scans the run's
  commits (`--redact`, so secrets never reach the report or the comment).
- **Licenses.** For each `Cargo.lock` or `package-lock.json` the run changed,
  the packages it added (new names or new versions) are checked against the
  policy. npm licenses come from the lockfile; Cargo licenses from
  `cargo metadata`. A denied license is critical, one not on a non-empty
  allowlist is high, a missing license is medium. `package-lock.json` v1 has
  no license data and is reported as unscanned.
- **Advisories.** `cargo audit` runs next to each changed `Cargo.lock` when
  cargo-audit is installed.

A scanner that is missing or fails never fails the run; the summary's
**Scanners** section says what did and did not run, so an empty findings list
is never mistaken for a full scan.

```toml
[pilot.security]
secrets_scanner  = "gitleaks"    # or a path to it; "" disables
dependency_audit = true          # cargo audit on Cargo.lock changes
allowed_licenses = ["MIT", "Apache-2.0", "BSD-3-Clause", "BSD-2-Clause",
                    "ISC", "MPL-2.0", "Unicode-DFS-2016"]  # [] = allow all not denied
denied_licenses  = []            # e.g. ["GPL-3.0", "AGPL-3.0"]
block_severity   = "medium"
```

## Escalation triggers

Some lines a run never crosses unseen, at any tier and with no switch to turn
them off. Every trigger is listed by `wingman pilot run`, written into the
escalation packet of a blocked run, and (except the 80% cost warning) blocks
auto-merge. The runtime ones (the first five rows) are also recorded as
`run.escalation` events the moment they fire, so they reach `state.json` and,
while workers are still running, a desktop card.

| Trigger | Fires when |
|---|---|
| Net-negative tests | A task reaches review with fewer passing tests than the same checks report at the base commit |
| Cost warn / halt | Spend reaches 80% / 100% of `max_usd` (the halt also stops in-flight workers) |
| 3 consecutive failures | The three runs before this one all failed (checked as it starts), or three worker attempts in a row fail |
| Irreversible task | A task classified `irreversible` runs |
| Force-push outside `wingman/auto/*` | Pushing the PR branch would need a force-push to a branch outside the pilot's namespace |
| Dangerous path, secrets, license header | The plan or the integration diff touches them (checked before the PR gate) |

**Test counts.** A `shell` or `run` check whose command mentions `test` (or
`jest`) is a test run. Its passing-test count is read from the runner's
summary: `cargo test` (every test binary, summed), `cargo nextest`, jest,
vitest, pytest, and `go test -v`. A command whose output carries none of
these is not counted, so a check that prints nothing recognisable never
trips the trigger. The base-commit count is measured once per check per run,
in the first worktree that needs it, before its worker starts; that worktree's
build is reused by the worker, so the added cost is one test run per distinct
check. Counts after the task come from the results the worker reports (the
same results the acceptance gate trusts) or from the supervisor's own re-run
when it re-verifies a worker that stopped without reporting.

**Force-pushes.** Every merge rebuilds the integration branch from the base
commit, so a resumed run whose branch was already pushed is rejected as
non-fast-forward. Inside `wingman/auto/*` the branch is replaced with
`--force-with-lease` pinned to the remote commit just read, but only when that
commit is one this clone has: a commit someone else pushed to the PR branch is
never overwritten, and the push fails for you to reconcile. Any other branch
is refused, and the run stops with an escalation packet instead of a PR.

**Retry history.** Each worker attempt records a `task.attempt` event (rung,
model, outcome, test counts). The escalation packet's "What was tried" section
lists them for the blocked task.

## Throughput and turn rollback

**Adaptive concurrency.** `max_concurrent_agents` is a ceiling, not a target.
Before each assignment the orchestrator narrows it from three signals:

- **Rate limits.** A worker reports every `429 Too Many Requests` and
  `529 overloaded` its provider answers, including ones the provider then
  retried, as an `agent.rate_limit` event. Each hit in the last minute takes
  25% off the headroom above one worker, and while a `Retry-After` is still
  running the cap is one.
- **Host CPU load**, re-read every 10 seconds (`/proc/stat` on Linux,
  `GetSystemTimes` on Windows, the load average on macOS): at 50% busy half
  the headroom is left. Where it cannot be read, it is ignored. Capability
  `adaptive_concurrency`, on at every tier.
- **Budget burn**: spend against `max_usd`.

A narrower cap never stops a running worker; the next assignment is refused
until the cap has room again.

**Speculative pre-spawn.** When every dependency of a waiting task is in
review or done, the orchestrator creates that task's worktree before the
manager assigns it and runs `turn_gate_cmd` there, so the worker starts on a
tree that is already built. Every worktree branches from the run's base
commit, so the early one is exactly what the assignment would create, and the
assignment takes it over (waiting for the warm-up to finish first). If the
plan changes first (the task is replanned onto work that has not reached
review, a dependency is sent back for rework, or the task is blocked or split
away) the warm-up is killed and the worktree and its branch are removed; the
same happens to any still unused when the run ends. Speculation only uses the
room the cap leaves once running tasks and unfinished warm-ups are counted.
Capability `speculative_prespawn`, on for copilot and autopilot.

**Turn rollback.** A worker's turn gate (`turn_gate_cmd`) feeds its failures
back to the model. With the `turn_rollback` capability (autopilot by default)
the worker also records the worktree's files each time the gate passes, and
after `turn_rollback_after` failures in a row restores that last green state
and tells the model its edits since then are gone. Before the gate has ever
passed, the tree the task started from is restored instead, but only if it
passes the gate; if the base is red as well, the edits are put back. Untracked
files count, ignored files and `.wingman/` do not, and HEAD is not moved.
Each rollback buys a fresh round of retries: the gate gets
`2 × turn_rollback_after` failures before the worker gives up.

```toml
[pilot]
turn_gate_cmd       = "cargo check --workspace"  # also warms speculative worktrees
turn_rollback_after = 2

[pilot.capabilities]
adaptive_concurrency = true    # host-load sampling (every tier)
speculative_prespawn = true    # copilot and autopilot
turn_rollback        = true    # autopilot only by default
```

## Merge conflicts

Tasks whose declared `writes` overlap never run at the same time, and each
task's branch is rebased onto the integration branch before it is squashed, so
most conflicts never happen. When a squash still conflicts, the run records a
`run.conflict` event and tries, in order:

1. **A one-shot rewrite.** The reviewer model is given each conflicted file,
   markers included, and its answer is kept only if no marker is left.
2. **Merge-fixer workers.** A `merge-fixer-<task>` task is recorded with the
   conflicted files as its `writes` and the conflicting task's acceptance
   checks, and a worker runs it in a worktree checked out at the integration
   branch tip with the conflicting task squash-merged in. When the worker
   reports done and no conflict marker is left in those files, its worktree's
   files become the conflicting task's squash commit and the merge carries on
   with the remaining tasks. Two attempts: the second gets the first's outcome
   and runs on the manager model. Capability `merge_fixer`, on for copilot and
   autopilot.

If neither resolves it, the run stops as it did before: the merge-fixer task
stays in the log (Failed, with its attempts in the escalation packet, or
pending when the capability is off) and `pilot resume` picks it up.

## Project knowledge

After every run that opens a PR, `.wingman/knowledge/` is brought up to date:

| File               | What                                                                                     |
| ------------------ | ---------------------------------------------------------------------------------------- |
| `hotspots.json`    | Per file, how often a landed task changed it and how often a merge conflicted on it, summed across runs from each run's `task.status` and `run.conflict` events |
| `architecture.md`  | The knowledge-keeper's architecture summary, above a module map from each `crates/*/src/lib.rs` |
| `decisions.jsonl`  | Architectural decisions, appended                                                        |

With the `knowledge_keeper` capability (autopilot by default) a model is given
the goal, each task's summary and changed files, the module map and the
current summary, and returns the revised summary plus up to three decisions the
run made. It runs on the `summarize` task class, so with
`[router.classes] summarize = "fast"` it uses `[router].fast_model`; with that
class unrouted it uses the manager's model. Without the capability, or when the
reply does not parse, the summary is left as it was and the run's goal is
appended as its decision.

The planner reads this layer back: its prompt carries the summary, the five
latest decisions, and the files earlier runs conflicted on most, with the
instruction to list any it edits in `writes` so the scheduler keeps those
tasks apart.

```toml
[router]
fast_model = "anthropic/claude-haiku-4-5"

[router.classes]
summarize = "fast"

[pilot.capabilities]
merge_fixer      = true    # copilot and autopilot
knowledge_keeper = true    # autopilot only by default
```

## Checkpoints, critic and first-try stats

**Checkpoint hygiene.** Workers have a `checkpoint` tool that commits the
worktree's changes (not `.wingman/`) onto the task branch, skipping commit
hooks, so a bad edit can be undone with git. With the `checkpoint_hygiene`
capability (autopilot by default) its use is enforced. The worker prompt
requires a checkpoint before a second file is edited and after each green
`run_acceptance`, and the worker supervisor checks the attempt's recorded tool
calls before letting the task into review. If the attempt edited a second file
before any checkpoint, the task is failed instead, even with every acceptance
check green. The reason goes to the next rung of the retry ladder. Only the
latest attempt's calls count, and single-file work is exempt. Before this
check, the gate ran at `finalize_task`, and the end-of-run merge skipped it for
tasks the manager never finalized. Without the capability the pipeline still
prints violations at the end of `pilot run`, but they block nothing.

**Critic.** With the `critic` capability (autopilot by default) a critic model
reads the plan before the approval gate. Up to three of its medium-or-worse
risks, worst first, become `guardrail-N` developer tasks that depend on every
planned task, so they are part of what you approve. The same critic runs again
before the auto-merge gate, where a high-or-worse risk vetoes the merge. It
runs on `critic_model`, then `reviewer_model`, then `default_model`, and each
can name its own provider. A critic from the workers' model family tends to
miss what they miss. `critic_other_family = true` makes that a hard rule: the
run refuses to start when the critic and `worker_model` share a family, or when
either family cannot be told from the model name (Claude, GPT/o-series,
Gemini/Gemma, Llama, Mistral, DeepSeek, Qwen, Grok, Kimi, GLM, Phi, Command).
A reply the critic cannot turn into a report adds no guardrails and vetoes
nothing.

**First-try stats.** Each run appends one record per task to
`~/.wingman/stats.jsonl`, and adaptive routing reads them back. `first_try_ok`
is true only when the task finished in review or done and every `task.attempt`
recorded for it ran on rung 0 without failing. A retry, an escalated model, a
split, or a reviewer rework all make it false. Before, a task counted as a
first-try success whenever it ended Done, however many rungs that took.

```toml
[pilot]
worker_model        = "anthropic/claude-haiku-4-5"
critic_model        = "openai/gpt-5"   # defaults to reviewer_model, then default_model
critic_other_family = true             # refuse a critic from the workers' family

[pilot.capabilities]
checkpoint_hygiene = true    # autopilot only by default
critic             = true    # autopilot only by default
```

## Post-merge feedback and evals

**Post-merge feedback.** A run that opened a PR learns how it ended: merged or
closed. `wingman pilot feedback` polls every such run's PR with `gh pr view`
and appends a `pr.outcome` event to its log once the PR is no longer open;
runs that already have one are skipped. `pilot daemon` runs the same pass on
the first cycle and then every `feedback_poll_secs` (default one hour, `0`
turns it off). The daemon checks between discovery cycles, so a cycle busy
with a dispatched run delays the pass, and the pass never runs more often than
`poll_interval_secs`. Before, only `pilot feedback` polled, and only when
someone ran it.

```toml
[pilot.daemon]
enabled            = true
poll_interval_secs = 300
feedback_poll_secs = 3600   # 0 = off
```

**Evals.** `wingman pilot eval --goals <file>` runs each goal through
`pilot run` with no PR, puts the checkout back on the branch it started from,
and scores the run on success (the run reached Done), cost, wall time and
quality. It writes `.wingman/eval/results.jsonl`, compares the
averages with the baseline (`--baseline <file>`, default
`.wingman/eval/baseline.json`), prints the report, and exits 1 when an axis is
more than `--threshold` (default 10%) worse. `--update-baseline` writes the
current results as the baseline instead. Without `--goals` it gates the
results already on disk.

A goals file has one goal per line. A plain line is a goal with no golden
reference. A JSON line can add one:

```
add a --version-only flag to wingman-cli
{"goal": "flush each session record in one write", "golden_commit": "8a9c0a9f293a4a0217c8208d71506c79b9b76928"}
{"goal": "rename the config key", "golden_diff": "golden/rename.diff", "base": "v0.3.0"}
```

- `golden_commit`: the commit's own change is the reference, and the run
  starts from its parent unless `base` says otherwise.
- `golden_diff`: a unified diff file, relative to the goals file.
- `base`: the revision the run starts from.

For a goal with a golden reference that reached Done, a judge model is given
the goal, the reference diff and the run's diff (base commit to integration
branch, each cut at 30,000 characters), and returns a score from 0 to 1. A run
that changed nothing scores 0. The judge runs on the `judge` task class, and
falls back to the planner model when that class is unrouted. Every other goal
is scored by the success proxy: 1.0 for Done, 0.0 otherwise. A reference that
cannot be read, a judge that cannot be built, or a reply that is not a score
in range also falls back to the proxy, and the reason is printed. The report
says how many goals on each side were judged, since judged and proxied scores
average together. Keep the judge model fixed, or quality moves with the judge.

```toml
[router.classes]
judge = "anthropic/claude-opus-4-7"
```

`.github/workflows/eval.yml` runs the suite in `eval/goals.jsonl` every Monday
and on demand, against `eval/baseline.jsonl`. It needs the `OPENROUTER_API_KEY`
secret and a `WINGMAN_EVAL_MODEL` variable (`WINGMAN_EVAL_JUDGE_MODEL` routes
the judge); without them it skips with a notice and passes. With no committed
baseline it gates nothing and says so. To set or move the baseline, commit the
`results.jsonl` from the `eval-results` artifact of a run you accept.

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

---

## The board

`wingman board` is a persistent, multi-project kanban board over pilot runs.
Where `pilot watch` shows one run closely, the board shows every goal across
every repo you've run pilot in — and its cards outlive the runs, so a backlog
survives what a run forgets.

```bash
wingman board                       # the TUI
wingman board add "<title>"         # a card in Backlog
wingman board dispatch <card>       # starts a pilot run for it
```

Columns are derived from the same `state.json` this document describes, so the
board and `pilot watch` cannot disagree. Press `o` on a card to hand off to
`pilot watch` for its newest run. See [BOARD.md](BOARD.md).
