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
> `dependabot`, `coverage_gaps` (reads an existing `lcov.info`),
> `intake`, and `pr_reviews` (review threads on pilot's own PRs — see
> [Review rounds](#review-rounds-on-pilots-prs)). **Intake** is transport-agnostic: a Slack/email gateway writes
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
> before enabling it. **Watch mode** (`pilot daemon --watch`) wakes the daemon
> on file saves and on git hooks that `pilot hooks install` writes. See
> [Watch mode](#watch-mode). The **container and vm sandbox tiers** run workers in
> Docker or a Firecracker microVM and apply their diff back, but are
> **unvalidated against a real daemon**; without a vm backend, pilot still
> refuses vm-tier tasks. See [Sandbox tiers](#sandbox-tiers).

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
wingman pilot export [<run-id>]   # the run as a PR description, with each worker's
                                  # files, receipts and tokens (secrets redacted)

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

## Review rounds on pilot's PRs

Add `pr_reviews` to `[pilot.daemon].sources` and the daemon answers review on
the PRs pilot opened, instead of leaving them for you to hand back:

```toml
[pilot.daemon]
enabled           = true
sources           = ["github_issues", "pr_reviews"]
trusted_authors   = ["your-github-login"]
auto_dispatch     = true    # without it, review rounds are only queued
max_review_rounds = 3       # per PR (default 3)
```

Each cycle it looks at every run on disk whose PR is still open on its
`wingman/auto/<run-id>` branch and lists the PR's unresolved review threads
(`gh api graphql`). A thread is work when a `trusted_authors` reviewer
commented on it since pilot last replied; comments from anyone else are
dropped — they never reach the rework prompt. With `auto_dispatch`, one round:

1. fetches the PR branch and runs a nested pilot run stacked on the PR's
   head, with the threads as its goal. It uses the same branch and opens no
   new PR;
2. pushes the new commits to that branch. The push is never forced, so if the
   branch moved in the meantime nothing is claimed. The rework does not go
   through the pipeline's merge gate, so if GitHub auto-merge is armed on the
   PR it is turned off first (and nothing is pushed if that fails);
3. replies on every thread it took on. A thread whose file the push changed
   gets the commits that touched it and is **resolved**; any other thread gets
   a reply saying so and **stays open**. Resolution follows what was pushed,
   not what the model says it did;
4. appends a `pr.review_round` event (round, rework run id, threads, the
   resolved ones, outcome, spend) to the log of the run that opened the PR.

Rounds stop at `max_review_rounds`, and together they share one
`[pilot].max_usd` budget per PR: each round gets what the earlier ones left.
A failed rework run still counts as a round, but it pushes nothing and
replies nothing. `wingman pilot daemon --dry-run` lists the rounds it would
start and does nothing else: no run, no push, no reply.

## Status

The full M1 pipeline is implemented (RunStore, planner, worker subprocess
with cross-platform supervisor, manager + orchestrator, git worktrees +
squash-merge, gh PR creation, dashboard, cost-cap enforcement, and the
provider-support gate). On top of that, the crate now ships the
`copilot`/`autopilot` machinery: a live control channel (`approve` /
`veto` / `abort` / `retry`), run `resume`, a per-run plan-approval gate,
sandbox tiers (`host` / `container` / `vm`: workers run in Docker or a
Firecracker microVM against a copy of their worktree; container degrades to
`host` without Docker, vm fails closed without Firecracker/KVM), and the
always-on discovery `daemon` (sources: GitHub issues, TODOs, CI failures,
Dependabot PRs, coverage gaps, file-drop intake, `// ASK:` comments, and review
threads on pilot's own PRs).
End-to-end `copilot` runs have been validated on a live provider
(OpenRouter/DeepSeek) — plan through PR; they need real API keys and are
**user-validated, not CI-validated** (CI runs the unit suite). Remaining
`autopilot`-only gaps: inbound Slack/email intake, live validation of the
container and vm sandbox tiers, and live-validated auto-dispatch.

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
| Claude Code  | `native`        | Your Claude subscription via the local `claude` CLI; Wingman tools over MCP. See [PROVIDERS.md](PROVIDERS.md). |
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

### Validating your providers

The tier column above is a static classification. To see which of *your*
providers actually carry a pilot run, run:

```bash
wingman pilot validate-providers                     # every [providers.*] section
wingman pilot validate-providers --provider anthropic --provider openrouter
```

For each provider it runs one canned plan: a single developer task that adds a
`--version-only` flag to a tiny `src/main.rs`, with a `grep` acceptance check.
The planner is skipped so every provider gets the same plan; the manager and
worker still have to use tool calls (`assign_task`, file edits,
`run_acceptance`, `task_complete`, `finalize_task`). Each run gets its own
scratch git repo under the system temp directory, `--no-pr`, no retries, no
reviewer or critic, and stays out of the adaptive-routing stats. The workers
are real `wingman --worker-mode` processes running `[providers.<id>].model`,
in the sandbox tier `[pilot.sandbox]` selects.

| Result    | Meaning                                                                                   |
| --------- | ----------------------------------------------------------------------------------------- |
| `pass`    | The run finished and `src/main.rs` on its integration branch contains `--version-only`.   |
| `fail`    | Anything else: a task failed or was aborted by the cap, the pipeline errored, or the flag never landed. The scratch repo is kept and its path is in the detail. |
| `skipped` | No model configured, no credential (config value, keyring or the provider's env var), an `unsupported` tier, or the provider could not be built (e.g. `[privacy].local_only`). Local servers need no credential, so they run and fail if nothing is listening. |

Each provider is capped by `--max-usd` (default $0.50) and `--max-tokens`
(default 400000, the bound that holds for models missing from the price
table). The spend lands in `wingman cost` like any pilot run. The matrix is
printed and written to `.wingman/provider-validation/matrix.md` and
`matrix.json` (`--out <dir>` to change). Exit status is 1 if any provider
failed, 2 if none could run, 0 otherwise.

Workers read the global config, so a provider defined only in a project's
`.wingman/config.toml` builds for the manager but not for its worker; that
row fails rather than passes.

---

## Watch mode

`wingman pilot daemon` polls every `poll_interval_secs`. With `--watch` it
still polls, since nothing local announces a new issue or a red CI run, but it
also wakes early when something happens in the repo:

| Event | Wakes after | Sources asked |
|-------|-------------|---------------|
| A file changes in the working tree | `watch_debounce_ms` of quiet | the local ones: `todos`, `coverage_gaps`, `intake`, `ask` |
| A `pilot hooks` git hook fires (`post-commit`, `post-merge`, `post-checkout`, `post-rewrite`) | `watch_debounce_ms` of quiet | every configured source |
| `poll_interval_secs` passes | | every configured source |

An event-woken cycle is an ordinary cycle: candidates are scored, deduplicated
against the queue, and trust and `max_auto_dispatch_per_cycle` apply
unchanged. `--cycles N` counts them too. If none of the local sources is
configured, file changes are ignored and only hooks and the poll wake it.

```toml
[pilot.daemon]
enabled           = true
sources           = ["github_issues", "todos", "intake", "ask"]
watch_debounce_ms = 1000   # raise it if an editor or build keeps waking the daemon
```

```bash
wingman pilot hooks install     # once per clone
wingman pilot daemon --watch
wingman pilot hooks uninstall   # removes only the hooks wingman wrote
```

**What doesn't wake it.** Changes under `.git/`, anything `.gitignore`
excludes (asked of `git check-ignore`, so build output doesn't trigger
cycles), and the daemon's own `.wingman/` state. The exceptions under
`.wingman/` are the intake directory, when the `intake` source is on, and
the hook signals.

**`ASK:` comments.** The `ask` source finds `// ASK: <question>` and
`# ASK: <question>` comments with `git grep`, untracked files included. Each
becomes the goal "answer it with a reply comment beside it, then remove the
marker". Under `--watch`, saving the file surfaces it within the debounce
window. ASKs are always proposals, never auto-run: the daemon cannot tell a
comment you typed from one that came in with a `git pull`.

**The hooks.** They are not shell scripts. Each hook's `#!` line is the path
of the wingman binary that installed it, so git runs wingman directly and
wingman writes a timestamp to `.wingman/watch/<hook>`, which the watcher
notices. Consequences:

- **Windows.** Git for Windows uses only the interpreter's file name and
  looks it up on `PATH`, so `wingman.exe` must be on `PATH`.
  `pilot hooks install` warns when it isn't.
- **Linux and macOS.** The kernel reads the path as written, so it can't
  contain whitespace. `install` refuses such a path rather than write a hook
  that never runs.
- **Moving the binary.** If you move or reinstall wingman somewhere else,
  run `install` again. It rewrites its own hooks.
- **Existing hooks.** A hook wingman didn't write (husky, pre-commit, your
  own) is left alone and reported as skipped. The hook directory comes from
  `git rev-parse --git-path hooks`, so `core.hooksPath` is respected.
- **Post-hooks only.** Git ignores their exit status, so a missing or broken
  wingman never blocks a commit.
- **Shared hook directories.** A hook records only in a repo that has
  `.wingman/watch/`, which `install` (and a watching daemon) creates. A
  `core.hooksPath` shared with other repos therefore never creates `.wingman/`
  in them.
- **Linked worktrees.** A hook firing in a linked worktree records nothing.
  Pilot's own task worktrees share the repo's hooks, and their commits are
  the daemon's own work.

**Limits.** Webhook-driven reactions from the original J13 design (a
dependabot PR going green, a labelled issue arriving) are not part of watch
mode. Those still arrive on the poll, or sooner through
[intake](#capability-tiers) or `wingman serve`'s goals endpoint. The watch is
one recursive watch over the whole repo, so on Linux a very large tree can
exhaust `fs.inotify.max_user_watches`.

---

## Sandbox tiers

> **Unvalidated against a real daemon.** Both backends are implemented and
> tested with mock runners (the `docker` and jailer/Firecracker argv, the
> Firecracker config, jail staging, the guest script, and patch-back with real
> git), but neither has been run against a real Docker daemon or a
> Firecracker/KVM host. Treat the first real run as a trial.

Each task gets a tier from its plan. Dependency or build files, or a risky
acceptance command (`npm install`, `curl`, `deploy`, ...), mean `container`;
migrations, infra, Dockerfile/terraform edits or an irreversible goal mean
`vm`. `[pilot.sandbox].default_tier` (or `pilot run --sandbox`) is a floor
under both.

| Tier        | Where the worker runs                        | When this machine has no backend for it |
| ----------- | -------------------------------------------- | --------------------------------------- |
| `host`      | its git worktree, on your machine            | n/a                                     |
| `container` | `docker run`, against a copy of the worktree | runs on the host (logged)               |
| `vm`        | a Firecracker microVM, against a copy        | **refused**: `pilot run` / `pilot resume` exit 2, and a vm-tier task the manager adds mid-run fails instead of starting. With `allow_unsandboxed_vm_tasks` it gets `container` if Docker is up, else host |

`wingman doctor` reports which tiers this machine can honour, and why not.

**How a sandboxed task runs.** The worktree, minus `.git`, is copied to a temp
directory along with a generated `.wingman-sandbox/run.sh`. The script commits
the copy as a base, runs `wingman --worker-mode` for the task, then writes
`git diff --binary` against that base (committed and uncommitted work alike)
followed by a completion line. If the worker reported `task_complete` and
exited cleanly, the diff is applied to the host worktree with `git apply` and
committed as `pilot(<task>): sandboxed worker changes`, where the squash-merge
picks it up. A failed attempt leaves the host worktree untouched, and the
supervisor does not re-run a sandboxed task's acceptance checks on the host to
salvage it, because that would run them on the host.

Two things do not come back: anything under the top-level `.wingman/` (the
worker's session transcript included), and the worker's own commit messages.
`.wingman/` and `.wingman-sandbox/` (which holds the copied global config) are
excluded when the patch is applied, so a worker that force-adds them still
cannot bring them back.

The patch is untrusted input. `git apply` refuses paths under `.git` or
through a symlink, a patch file replaced by a link is refused, a patch without
its completion line (a failed or truncated diff) is refused rather than
half-applied, and a VM's patch is read off a raw drive, so no guest filesystem
is parsed on the host.

```toml
[pilot.sandbox]
default_tier     = "host"                    # floor: host | container | vm
container_image  = "wingman/sandbox:latest"  # needs sh, git and a Linux `wingman`
cpus             = 2                         # docker --cpus / Firecracker vcpu_count
memory_mib       = 4096                      # docker --memory / mem_size_mib
pids_limit       = 512                       # docker --pids-limit
network          = "bridge"                  # bridge | none | a network you created
env              = ["ANTHROPIC_API_KEY"]     # forwarded into the sandbox by name
allow_unsandboxed_vm_tasks = false

[pilot.sandbox.vm]
firecracker_bin    = "/usr/local/bin/firecracker"  # absolute when use_jailer
use_jailer         = true                          # the jailer needs root
jailer_bin         = "jailer"
chroot_base_dir    = "/srv/jailer"
jailer_uid         = 65534
jailer_gid         = 65534
kernel_image       = "/var/lib/wingman/vmlinux"     # empty = vm tier unavailable
rootfs_image       = "/var/lib/wingman/rootfs.ext4"
worktree_drive_mib = 4096
tap_device         = ""                            # pre-created tap; empty = no NIC
```

**Credentials and network.** The worker calls its model provider from inside
the sandbox, so it needs both. The global `config.toml` is copied in, but keys
kept in the OS keyring are out of reach: list the provider's key variable in
`env`. A container is attached to `network`, so `"none"` leaves the worker
unable to reach a hosted provider; a network of your own with egress rules is
the useful middle. A VM has no NIC unless `tap_device` names one you created
and routed. Env values forwarded into a VM are written into the guest script
on the worktree drive, which is deleted when the task ends.

**Container image.** Wingman does not publish one: it needs `sh`, `git` and a
Linux `wingman` on `PATH`. The container gets `--security-opt
no-new-privileges` and the CPU, memory and pid limits above; on Linux it runs
as your uid so its files stay removable. It is removed with `docker rm -f` when
the task ends, including on timeout.

**VM backend** (Linux only). It needs a read-write `/dev/kvm`, `firecracker`
(and `jailer` with `use_jailer`), `mke2fs` from e2fsprogs 1.43 or later, and a
kernel and rootfs you provide. The guest sees `/dev/vda`, the rootfs
(read-only); `/dev/vdb`, the worktree copy as ext4; and `/dev/vdc`, a raw
64 MiB drive for the patch. The kernel is booted with
`init=/sbin/wingman-sandbox-init`, which the rootfs must provide, along these
lines:

```sh
#!/bin/sh
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs tmp /tmp
mkdir -p /work && mount /dev/vdb /work
sh /work/.wingman-sandbox/run.sh </dev/console >/dev/console 2>&1
sync; reboot -f
```

Firecracker exits when the guest reboots. The worker's NDJSON reaches pilot
over the serial console, which is Firecracker's stdout; kernel messages on the
same console are ignored. The guest's exit code does not reach the host, which
is what the patch's completion line is for.

## Skill packs

A skill pack is a versioned bundle of role definitions (`<role>.md`), lessons
(`<role>.lessons.md`), tool registrations (`tools/`) and acceptance templates,
published as a git repo tagged `v<X.Y.Z>`. Installing one copies its roles and
lessons into `~/.wingman/agents/`, where the role loader picks them up.

```toml
[pilot.skills]
packs = ["acme/rust-reviewer@1.4"]                   # caret requirements
index = "https://github.com/acme/wingman-packs"      # git URL or local dir
```

```bash
wingman pilot skills install                # [pilot.skills].packs
wingman pilot skills install acme/app@1.0   # or name them
wingman pilot skills search reviewer
wingman pilot skills list
wingman pilot skills verify                 # non-zero exit on any failure
```

**Index.** `index` names a git repo (shallow-cloned on each use) or a local
directory holding `index.json`:

```json
{"packs": {"acme/app": [
  {"version": "1.2.0", "source": "https://github.com/acme/app",
   "deps": ["acme/base@1.3"], "description": "App roles",
   "signature": "-----BEGIN SSH SIGNATURE-----\n...\n-----END SSH SIGNATURE-----\n"}
]}}
```

**Dependencies.** Each requirement is caret-style: `acme/base@1.3` accepts any
`1.x` at or above `1.3.0`. Install resolves the requested packs and everything
they depend on, choosing the newest indexed version that satisfies every
requirement on a pack, and stops with a `version conflict` error naming the
requirements when none does. One version per pack, because every pack's roles
share one agents directory. The resolver does not backtrack, so in a rare
case it reports a conflict that a different choice upstream would have
avoided.

**Signatures.** Unsigned packs are refused unless you pass
`--allow-unsigned`; a pack that *is* signed must verify either way. Without an
`index`, packs are cloned from `https://github.com/<owner>/<name>` and are
always unsigned. Checking uses `ssh-keygen -Y verify` (OpenSSH 8.1+, which
ships with Git and with Windows 10+) against
`~/.wingman/packs/allowed_signers`, with the pack **owner** as the principal,
so a key you trust for `acme` cannot vouch for `evil/…`:

```
acme namespaces="wingman-skillpack" ssh-ed25519 AAAAC3Nza...
```

The signed message covers the exact pack version, a SHA-256 digest of its
files (`.git` excluded, symlinks refused, checked out with
`core.autocrlf=false`) and its dependency list, so neither the source nor the
index can change what was signed. Content that fails the check is deleted.
Each install leaves a receipt, `~/.wingman/packs/<slug>.json`; `verify`
re-hashes the installed files against it, so later edits on disk are caught
too, including for unsigned packs.

**Publishing.** Sign a clean checkout of the tag. Write the payload with
`--out` rather than a shell redirect, which can re-encode it:

```bash
git clone --branch v1.2.0 https://github.com/acme/app app
wingman pilot skills digest acme/app@1.2.0 app --dep acme/base@1.3 --out payload.txt
ssh-keygen -Y sign -f ~/.ssh/id_ed25519 -n wingman-skillpack payload.txt
# put the text of payload.txt.sig in the index entry's "signature"
```

## Tool synthesis

A worker that keeps needing a command the toolset lacks — querying the dev
database, regenerating a fixture — can propose it as a named tool with
`propose_tool`. The proposal is an ordinary custom command tool, written to
the owning project (not the worker's worktree, which is deleted after the
task):

```toml
# .wingman/tools/query_db.toml
name = "query_db"
description = "Run a read-only SQL query against the dev database"
command = "python scripts/query_db.py"   # reads $WINGMAN_TOOL_INPUT
timeout_secs = 20
```

Once approved, every registry built from then on carries it: the next worker
spawned in this run, later runs, and interactive sessions in the project. The
proposing worker does not get it mid-task.

**Turning it on.** The `tool_synthesis` capability is on by default for
`autopilot` only; turn it on (or off) for any tier with

```toml
[pilot.capabilities]
tool_synthesis = true
```

**Approval.** A tool is approved when its exact file content is recorded in
the trust store (`~/.wingman/trusted.toml`, the same store `wingman trust`
uses), so editing an approved file revokes it.

| Run | Gate |
|-----|------|
| `autopilot`, and the project config is trusted (`wingman trust`) | auto: the proposal approves itself |
| anything else | hard gate: waits for you |

```bash
wingman pilot tools                  # list, with [approved] / [pending]
wingman pilot tools approve query_db # prints the command it approves
wingman pilot tools reject query_db  # deletes it and its trust record
```

There is no notify-only band: a synthesized tool runs shell commands in every
later session, and a veto window during an unattended run is not a gate.

**The ceiling.** A synthesized tool is a name for a command the worker could
already run, never more:

- `propose_tool` needs the shell permission and refuses a command the shell
  denylist blocks.
- A synthesized tool runs through `run_shell`'s guards —
  `[tools].shell_sandbox` (including `required`), credential scrubbing, the
  Windows Job Object — unlike a user-defined `[[tools.custom]]` entry. Its
  input arrives in `$WINGMAN_TOOL_INPUT` only, not on stdin.
- It never replaces a tool already registered, and a file whose name does not
  match its `name` is ignored.
- None load when `run_shell` is removed by `[tools].disabled_tools` or a
  preset.

**Limits.** Workers in the container and vm [sandbox tiers](#sandbox-tiers)
do not get `propose_tool`: they run against a copy with no `.wingman/` and
no trust store, so a proposal could not come back. They do not see approved
tools either. A proposal names a command that already works; nothing writes a
tool's implementation for it. Unvalidated against a live provider: the
worker-side flow is covered by unit tests only.

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
