# Arc-Code in CI

Arc-Code runs entirely inside your own CI runner — the only thing that
leaves your environment is the model API traffic to the provider you
configure. That makes it a self-hosted alternative to cloud review bots for
teams that can't ship code to a vendor's cloud.

## GitHub Action: PR review

The repository ships a composite action (`action.yml` at the repo root).
Add a workflow like this to any project:

```yaml
# .github/workflows/arccode-review.yml
name: Arc-Code review
on:
  pull_request:
    types: [opened, synchronize]

permissions:
  contents: read
  pull-requests: write

jobs:
  review:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: vedantnimbarte/ArcCode@main
        with:
          model: anthropic/claude-sonnet-4-6
          api-key: ${{ secrets.ANTHROPIC_API_KEY }}
```

The action builds Arc-Code once per runner (cached), reviews the PR diff
with the configured model, and posts the findings as a PR comment using the
workflow's own `GITHUB_TOKEN`.

### Inputs

| Input | Required | Description |
|---|---|---|
| `model` | yes | `provider/model` to review with |
| `api-key` | yes | Provider API key (use a repo/org secret) |
| `pr-number` | no | Defaults to the triggering pull request |
| `arccode-version` | no | Arc-Code git ref to build (default `main`) |

### Notes

- Use a cheap, fast model for routine PRs (`anthropic/claude-haiku-4-5-20251001`)
  and a stronger one for release branches — workflows are just YAML, so split
  by branch filter.
- Multi-model review is available locally via `arccode review-multi`; wire it
  the same way if you want consensus findings in CI.
- The build uses `--no-default-features` (no ONNX/tree-sitter) to keep CI
  build times reasonable; review quality is unaffected since reviewing reads
  the diff, not the semantic index.

## Headless mode for custom pipelines

For anything beyond review, drive Arc-Code headlessly:

```bash
arccode --print "summarize the risk in this diff" --json < pr.diff
```

`--json` emits one JSON event per line (the same `AgentEvent` stream the TUI
consumes), so you can script arbitrary gates — block a merge when the agent
flags a migration risk, generate changelog entries on tag builds, etc.
