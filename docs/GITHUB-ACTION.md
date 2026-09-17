# GitHub Action

`uses: vedantnimbarte/Wingman@<tag>` runs Wingman inside a workflow, with no
CLI to install by hand:

- **Open a PR** → Wingman posts an inline review (`wingman review <pr> --comment`).
- **Comment `@wingman …` on a PR** → Wingman addresses the review comments and
  failing checks on the PR branch (`wingman pr address <pr>`) and pushes the fix.
- **Comment `@wingman …` on an issue** (or open an issue that mentions it) →
  Wingman works on branch `wingman/issue-<n>` headlessly. If the run exits
  green and changed something, it pushes and opens a PR that closes the issue.
  If the run ends red, it comments on the issue with the last lines of output
  and opens nothing.

> **Status: not validated live.** This action has never run on a real GitHub
> runner. What has been checked: the event → mode resolution against fixture
> payloads (`scripts/action-selftest.sh`, also run in CI), `shellcheck` on the
> scripts, `actionlint` on the example workflow below, and a local smoke run of
> the issue/PR flows against a bare git remote with `wingman` and `gh` stubbed
> out. The real `gh pr create`, pushes with `GITHUB_TOKEN`, and a real model
> run inside Actions are all unexercised. Please report what breaks.

It is a composite action: it installs the release binary with
[`scripts/install.sh`](../scripts/install.sh) (checksum-verified) from the same
ref you pinned, then runs [`scripts/action.sh`](../scripts/action.sh). Linux
and macOS runners only; on Windows runners it fails with an error.

`action.yml` is not in any published release yet (v0.4.0 and earlier lack it):
pin the first tag that contains it, and set `wingman-version` to the same tag
so the binary has the `review --comment` and `pr address` behaviour described
here.

## Example workflow

`.github/workflows/wingman.yml`:

```yaml
name: wingman

on:
  pull_request:
    types: [opened, synchronize, ready_for_review]
  issue_comment:
    types: [created]
  issues:
    types: [opened]

# One run per issue/PR at a time. A new mention waits for the running one
# (GitHub keeps only the newest pending run per group).
concurrency:
  group: wingman-${{ github.event.issue.number || github.event.pull_request.number }}
  cancel-in-progress: false

permissions:
  contents: write       # push wingman/issue-<n> and PR fix commits
  pull-requests: write  # post reviews, open PRs
  issues: write         # comment on issues
  checks: read          # `pr address` reads failing checks
  statuses: read

jobs:
  wingman:
    # Cheap pre-filter so unrelated comments don't boot a runner. The action
    # re-checks the trigger and the author itself; this is not the security gate.
    if: >-
      github.event_name == 'pull_request' ||
      contains(github.event.comment.body, '@wingman') ||
      contains(github.event.issue.body, '@wingman') ||
      contains(github.event.issue.title, '@wingman')
    runs-on: ubuntu-latest
    timeout-minutes: 30  # the real spend cap, see max-usd below
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false  # required: see "Security model"
      - uses: vedantnimbarte/Wingman@v0.5.0  # pin a tag or, better, a commit SHA
        with:
          provider-key: ${{ secrets.ANTHROPIC_API_KEY }}
          model: anthropic/claude-opus-4-7
          wingman-version: v0.5.0
```

Only want reviews? Drop the `issue_comment`/`issues` triggers and set
`mode: review`. Only `@wingman` on issues? Drop `pull_request`.

## Inputs

| Input | Default | What it does |
| --- | --- | --- |
| `mode` | `auto` | `auto` picks from the event (table below). `review`, `address` act only on PR events; `fix` only on issues. |
| `provider-key` | (required) | The provider's API key. Exported as `WINGMAN_<PROVIDER>_API_KEY`, masked, never echoed. |
| `model` | (required) | `provider/model`, e.g. `anthropic/claude-opus-4-7`, `openrouter/deepseek/deepseek-chat`. The prefix picks which provider the key is for. |
| `trigger` | `@wingman` | Substring a comment (or new issue's title/body) must contain. Not checked for `pull_request` review. |
| `permission-mode` | `auto-edit` | Mode for `fix` runs. `review` is always read-only and `address` always `auto-edit` (both are fixed by those commands). |
| `max-usd` | empty | Writes `[tokens].max_usd_per_session`. **Headless runs only warn** when the estimate crosses it; they do not stop. Use `timeout-minutes` as the hard cap. |
| `wingman-version` | empty (latest) | Release tag to install. Pin it. |
| `allowed-associations` | `OWNER,MEMBER,COLLABORATOR` | Who may trigger the action, by `author_association`. |
| `github-token` | `github.token` | Used for `gh` and `git push`. Note that pushes and PRs made with the default `GITHUB_TOKEN` do not trigger other workflows, so CI will not run on Wingman's PR until someone pushes to it; pass a GitHub App or fine-grained token if you need that. |

| Event | `auto` does |
| --- | --- |
| `pull_request` | `wingman review <n> --comment`, if the PR author is allowed |
| `issue_comment` on a PR, containing the trigger | `wingman pr address <n>` on the PR branch, then commit + push |
| `issue_comment` on an issue, containing the trigger | headless fix on `wingman/issue-<n>` → PR, or a comment on red |
| `issues` whose title/body contains the trigger | same as above |
| anything else, bots, or a disallowed author | logs a notice and exits 0 |

"Green" means `wingman --print` exited 0. That exit is non-zero when a tool
errors fatally or the verification gate stops the turn red (exit 2); note that
a run that stops on `max_turns` or `max_tokens` still exits 0. The gate is only
as strong as what it detects in your repo (see "Verification" below).

## Security model

This action is a trust boundary: it turns text written on GitHub into an agent
with shell access and a token that can push. What it does about that:

- **Who can trigger it.** It acts only when the actor's `author_association`
  is in `allowed-associations` (default `OWNER`, `MEMBER`, `COLLABORATOR`):
  the commenter for `issue_comment`, the issue author for `issues`, the PR
  author for `pull_request`. Events from bots are ignored. Caveat: GitHub can
  report an org member with *private* membership as `CONTRIBUTOR`; make the
  membership public or add them as a collaborator.
- **No expression injection.** Nothing from `github.event.*` is interpolated
  into a `run:` script. `action.yml` passes only the action's inputs through
  `env:`; the script reads comment/issue text from `$GITHUB_EVENT_PATH` with
  `jq` and passes it to commands as quoted arguments. Your workflow's
  `if:` may use `contains(github.event…)` safely: that is an expression, not a shell.
- **Forks.** `address` never pushes to a PR whose head is in a fork (it checks
  `isCrossRepository` and skips). Fork PRs under `pull_request` get no secrets,
  so a review of one fails with "provider-key is empty". Do **not** switch to
  `pull_request_target` to work around that.
- **Credentials the agent can reach.** The run refuses to start if the checkout
  persisted a token into git config (`persist-credentials: false` is
  required), because `.git/config` is readable by the agent. The token is
  passed to `git fetch`/`git push` per command instead. Wingman's `run_shell`
  tool strips `*_API_KEY`, `*_TOKEN` and `AWS_*` from every command it runs, so
  the provider key and `GH_TOKEN` in the environment are not visible to shell
  commands the model issues.
- **The repo's own Wingman config is untrusted.** The action points
  `WINGMAN_HOME` at a fresh directory under `$RUNNER_TEMP`, so the trust store
  (`trusted.toml`) is empty and `.wingman/config.toml` loads as an untrusted
  project layer: `hooks`, `mcp`, `verify`, `providers` (base URLs),
  `permission_mode`, `pilot` and friends are dropped with a warning, exactly as
  they are for a freshly cloned repo on your machine. `wingman trust` is never
  run. Keys that only select a model or narrow behaviour (`default_model`,
  `tokens`, `router`, `git`, `tools.disabled_tools`, …) still apply; note
  `WINGMAN_MODEL` from the `model` input overrides the repo's `default_model`.
  A `.claude/settings.json` in the repo is not imported either.
- **What an allowed person vouches for.** The prompt includes content the
  trigger author did not write: for an issue fix, the issue title and body
  (possibly written by anyone); for `pr address`, every review and comment on
  the PR (the command does not filter authors); for review, the PR diff. An
  `@wingman` mention is a decision to hand that text to an agent running in
  `auto-edit`, which can run shell commands on the runner. Read it first.
  Output posted back (the red-run comment, the review) goes to the same
  issue/PR it came from.
- **Blast radius.** Pushes go only to `wingman/issue-<n>` (force-pushed, so a
  rerun replaces the last attempt) or to the same-repo PR branch that was
  mentioned. Nothing merges. Protect `main` with required reviews.

Minimum `permissions:` for all three flows is the block in the example:
`contents: write`, `pull-requests: write`, `issues: write`, plus `checks: read`
and `statuses: read` so `pr address` can see failing CI (without them it
silently sees no failing checks; this exact set is unverified on a runner). Review-only
workflows need `contents: read` and `pull-requests: write`.

## Verification

Wingman's verification gate runs after edits with `turn_gate = "auto"`. In the
action the repo's `[verify]` section is ignored (it is an untrusted key), so
only auto-detection applies. If your project needs a toolchain (Rust, Node,
Python deps) to build and test, set it up in steps before the Wingman step, or
the gate cannot tell green from red.

## What is deliberately left out

- Pilot mode (`wingman pilot run`): the action runs one headless agent per event.
- A hard spend cap: see `max-usd` above.
- Windows runners, `pull_request_review_comment` events, and slash-style
  commands (`@wingman review` vs `@wingman fix`): the event decides the mode.
