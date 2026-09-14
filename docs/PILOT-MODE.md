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
sandbox tiers (`host` / `container` / `vm`: workers run in Docker or a
Firecracker microVM against a copy of their worktree; container degrades to
`host` without Docker, vm fails closed without Firecracker/KVM), and the always-on discovery `daemon` (five
sources: GitHub issues, TODOs, CI failures, Dependabot PRs, coverage gaps).
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
unchanged. `--cycles N` counts them too.

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
| `vm`        | a Firecracker microVM, against a copy        | **refused**: `pilot run` / `pilot resume` exit 2. With `allow_unsandboxed_vm_tasks` it gets `container` if Docker is up, else host |

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
