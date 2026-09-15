# 0022 — Skill packs are verified with `ssh-keygen`, not a signature crate

**Status:** accepted
**Date:** 2026-09-15

## Context

J12 skill packs install agent roles from a registry index, and a pack's roles
end up in every pilot run. Packs therefore need signatures, and a signature
check needs keys the user already trusts and a verifier.

## Decision

`pilot skills install|verify` shells out to `ssh-keygen -Y verify` with
`~/.wingman/packs/allowed_signers`, the pack owner as principal and the
namespace `wingman-skillpack`. The signed payload is produced by
`wingman pilot skills digest`, so author and installer build it with the same
code.

## Why not a crate (ed25519-dalek, minisign, sigstore)

- **Key distribution is the hard part, and SSH already solved it.** Authors
  have SSH signing keys (GitHub displays them); `allowed_signers` is a format
  users can read and edit, with per-principal namespaces that stop a key
  trusted for `acme` from vouching for `evil`.
- **No new dependency.** A crypto crate is a supply-chain surface this repo
  would have to audit under `deny.toml`, for a check that runs a handful of
  times per install.
- **OpenSSH 8.1+ ships with Git for Windows, Windows 10+, macOS and every Linux
  CI image**, so the verifier is present wherever `git` already is.

## Consequences

Verification needs `ssh-keygen` on `PATH`; without it a signed pack fails
closed with an error naming the requirement. The format is fixed by OpenSSH,
so if a future registry wants keyless or transparency-log signing (sigstore),
that is a new record superseding this one, not a second verifier bolted on
beside it.
