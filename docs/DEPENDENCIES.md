# Dependency Notes & Tracked Debt

Known dependency issues, why they exist, and the conditions under which they
resolve. The `deny` CI job (see `.github/workflows/ci.yml` + `deny.toml`) keeps
these visible; the `audit` job surfaces new advisories. Both are informational
until the items below clear, then flip `deny.toml`'s `[bans] multiple-versions`
to `"deny"`.

## `reqwest` — migrated to 0.13 (first-party unified)

**Status:** done for everything we control. All five first-party crates
(`wingman-cli`, `wingman-mcp`, `wingman-providers`, `wingman-tools`,
`wingman-tui`) are now on **reqwest 0.13**, sharing one copy with `rmcp` (the
MCP client, which was already on 0.13). The `rustls-tls` feature was renamed to
`rustls` in 0.13, and that feature now pulls `aws-lc-rs` (needs `cc`/NASM) — so
we instead use `rustls-no-provider` and install the **ring** crypto provider
once at startup (`wingman_core::ensure_tls_provider`), preserving the
static-binary / no-OpenSSL distribution story. `.form()` moved behind reqwest's
`form` feature (enabled on `wingman-providers` for the watsonx IAM exchange).

**Remaining 0.12 copy — outside our control:** `hf-hub` (pulled by `fastembed`
behind the optional `embeddings` feature of `wingman-rag`, for downloading
embedding models) still depends on `reqwest 0.12`. So a second reqwest major
persists *only* in the embeddings dependency tree. It clears when `hf-hub` bumps
to 0.13, or entirely if you build `wingman-rag` with `--no-default-features`
(hash embedder, no fastembed). `deny.toml` keeps `multiple-versions = "warn"`
until then.

## `ort` release-candidate (`2.0.0-rc.x`)

**Status:** tracked; not directly in our control.

`ort` (ONNX Runtime bindings) is pulled by `fastembed` for local embeddings
behind the `embeddings` feature of `wingman-rag`. It is a release candidate in a
shipping product, and its prebuilt binary dictates the **glibc ≥ 2.38** floor
documented in the CI/release comments.

**Resolution:** upgrade when `fastembed` ships a stable `ort`. We don't depend on
`ort` directly, so we can't pin ahead of `fastembed`. The hash-embedder fallback
(`--no-default-features` on `wingman-rag`) avoids `ort` entirely for builds that
don't need semantic search.

## Policy

New advisories or new duplicate majors should be triaged here with a crate name,
a reason, and a resolution condition — not silently ignored. Add advisory
ignores to `deny.toml`'s `[advisories] ignore` only with a comment linking to
this file.
