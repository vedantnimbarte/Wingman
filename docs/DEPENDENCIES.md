# Dependency Notes & Tracked Debt

Known dependency issues, why they exist, and the conditions under which they
resolve. The `deny` CI job (see `.github/workflows/ci.yml` + `deny.toml`) keeps
these visible; the `audit` job surfaces new advisories. Both are informational
until the items below clear, then flip `deny.toml`'s `[bans] multiple-versions`
to `"deny"`.

## Duplicate `reqwest` (0.12 + 0.13)

**Status:** tracked, low-priority (build bloat only, not a correctness issue).

Every wingman crate pins `reqwest = "0.12"`. The second major (`0.13`) is pulled
transitively by `rmcp` (the MCP client), which moved to `reqwest 0.13`:

```
reqwest v0.13.4
└── rmcp v1.7.0
    └── wingman-mcp
        └── wingman-cli
```

Unifying is **not** a free bump: `reqwest 0.13` renamed the `rustls-tls` feature
to `rustls` and carries API changes, so migrating our five direct dependents
(`wingman-cli`, `wingman-mcp`, `wingman-providers`, `wingman-tools`,
`wingman-tui`) is a real, wide change driven entirely by a transitive dep.

**Resolution paths (either clears it):**
- Bump the whole stack to `reqwest 0.13` once we're ready to migrate features +
  API and re-verify the MSRV job — do this deliberately, not as a side effect.
- Or wait for `rmcp` to align, or pin `rmcp` to a `reqwest 0.12` release if one
  exists and we can accept that version.

Until then: `deny.toml` sets `multiple-versions = "warn"` so it's visible but
non-blocking.

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
