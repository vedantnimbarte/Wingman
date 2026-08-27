# Contributing to Wingman

Thanks for taking the time. This document covers what you need to build, what
the review bar is, and one policy that matters more than usual for this project.

## Building

Wingman is a Rust workspace. You need the toolchain at or above the declared
MSRV (`rust-version` in the root `Cargo.toml`, currently 1.88).

```sh
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

All four must pass. CI runs them on Linux, macOS, and Windows, plus an MSRV
check — `-D warnings` means a clippy lint failure is a build failure.

Some optional features need extra setup:

- `--features browser` needs a Chrome/Chromium binary.
- The LSP tools need the relevant language server on `PATH`. They degrade
  gracefully when it is missing, so tests do not require one.
- Semantic search downloads an embedding model on first use.

`wingman doctor` reports what is and isn't available on your machine.

## AI-generated contributions

Wingman is an AI coding agent, so we will receive AI-generated pull requests.
That is fine — we would be poor advocates for the tool otherwise. Two rules:

1. **Disclose it.** Say in the PR description that an agent wrote some or all of
   the change, and which one. This is not a demerit; it is context for review.
2. **You are the author.** Understand the diff, be able to explain why it is
   correct, and have run the tests. "The agent said it works" is not a review
   response we can act on.

PRs that are clearly unreviewed model output — plausible-looking changes with no
tests, sweeping unrelated reformatting, or descriptions that do not match the
diff — will be closed without detailed review. This is not hostility toward the
tooling; it is that reviewing them costs more than writing the change.

If you are using Wingman itself to contribute, `wingman explain` produces a
per-file walkthrough of your working diff that makes a good PR description
starting point.

## Pull requests

- **One concern per PR.** A bug fix and a refactor in the same diff take three
  times as long to review.
- **Tests for behaviour changes.** Especially anything touching permissions,
  path containment, or the config trust boundary — those have a regression test
  each for a reason, and new ones are the cheapest way to keep them.
- **Explain the why in the commit message.** The diff shows what changed; the
  message should say what was wrong before. See `git log` for the house style.
- **Write a decision record when the reasoning outlives the diff.** If the
  boundary looks arbitrary but is load-bearing, if you rejected a plausible
  alternative, or if you deferred something with a trigger, add a note under
  [docs/decisions/](docs/decisions/README.md). Not for ordinary fixes — the
  commit message is still the default, and that README says when each applies.
- **Match the surrounding code.** Comment density, naming, and error-message
  phrasing are fairly consistent; follow the file you are editing.

## Security

Do not open a public issue or PR for a security problem. See
[SECURITY.md](SECURITY.md) for the private reporting channel and the threat
model — the latter is worth reading before reporting, since several
alarming-looking behaviours are intentional and documented.

Changes to the security-relevant surfaces get closer review:

- `crates/wingman-tools/src/ctx.rs` — permission predicates, path containment
- `crates/wingman-tools/src/registry.rs` — the central capability gate
- `crates/wingman-config/src/lib.rs` — the project-config trust boundary
- `crates/wingman-config/src/trust.rs` — trust records
- anything that spawns a process or opens a listener

If you add a tool, declare its `capabilities()` honestly. The default is
`Capability::NONE`, which means "pure computation" — a tool that touches the
filesystem, shell, or network and does not say so is a bug, and the gate will
not catch it for you.

## Filing issues

Include the version (`wingman --version`), your OS, the permission mode, and
what you expected versus what happened. `wingman doctor` output is useful for
anything environment-related.
