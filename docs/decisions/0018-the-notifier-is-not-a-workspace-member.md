# 0018 — The desktop notifier is excluded from the workspace

**Status:** accepted
**Date:** 2026-09-05

## Context

The desktop notification popup ([0017](0017-notifications-are-a-file-inbox.md))
is a Tauri v2 app: a Rust binary plus a React frontend, at `desktop/notifier/`.
Tauri was chosen over Electron on bundle size (~4–8 MB against ~85–120 MB plus a
second runtime) and because it uses the system webview — WebView2 ships in-box
on Windows 11, WKWebView on macOS — so the tail logic can mirror `control.rs`
directly instead of being reimplemented in another language.

The obvious thing is to add it to `members` in the root `Cargo.toml`. That
breaks three CI jobs, two of them required:

| Job | Required | Breaks how |
|---|---|---|
| `matrix-test` | yes | needs libwebkit2gtk on all three runners |
| `msrv` | yes | same, plus Tauri's tree may outrun Rust 1.88 |
| `audit` | **yes** | ~300 more crates in the root lock; any RUSTSEC advisory anywhere in the webview tree turns a required gate red |
| `deny` | no | `multiple-versions` explosion |

`audit` is decisive on its own: a required gate that fails on an advisory in a
transitive webview dependency, on a surface nobody has to build to use Wingman,
is a gate that gets ignored.

`default-members` does not help — `--workspace` ignores it, and that is what CI
runs. A cargo feature does not help either: an optional dependency still lands
in `Cargo.lock`, so `cargo audit` still sees it, and a feature flag cannot gate
*system* libraries. Every Linux contributor running the four gates from
`CONTRIBUTING.md` would need the webview headers too.

## Decision

`desktop/notifier` has its own `[workspace]` table and its own `Cargo.lock`, and
the root manifest lists it under `exclude`. Build it with `--manifest-path`.

This is the arrangement `editors/vscode` already has for npm: a separate
surface, a separate toolchain, a separate lockfile, and its own CI job — which
is informational, for the same reason the `vscode` job is.

It does **not** depend on `wingman-config`, even though that is where the wire
format lives. Taking it would drag `keyring` — and libsecret/D-Bus on Linux —
into a binary whose whole point is being cheap to build. The ~90 lines of serde
structs and byte-offset tailing are copied instead, and `directories` supplies
`~/.wingman`. The same goes for the design tokens copied from `panel/src/app.css`.

## Consequences

- **The copies must not drift.** Two tests hold them: an identical
  `encoding_is_the_documented_shape` assertion on each side of the wire format,
  so a serde rename fails a build rather than the popup at runtime; and
  `tokens.test.ts`, which reads the panel's CSS and the popup's and compares the
  custom properties.
- **`cargo-deny`, `cargo-audit` and Dependabot's cargo ecosystem do not cover
  `desktop/notifier/Cargo.lock`.** This is the accepted cost of the exclusion.
  If the notifier ever ships in the default install, that gap has to be closed
  first.
- The notifier's own CI job installs the webview libraries and runs clippy and
  tests against the detached manifest. It is not in the `test` aggregate's
  `needs`, so it cannot block a Rust PR.
- Note the asymmetry with `web-ui`, which *is* required: `build.rs` deliberately
  substitutes a placeholder page when `panel/dist` is missing, so a broken panel
  build would otherwise ship a green binary serving "the web UI was not built".
  Tauri's `frontendDist` has no such fallback — a missing `ui/dist` fails the
  build loudly — so there is nothing here to catch.
- v1 ships no installers and no code signing. Bundling for three platforms,
  a Windows OV certificate (weeks of procurement, HSM-backed since June 2023)
  and macOS notarization were roughly half the total effort for a feature whose
  value is fully delivered by "the binary exists and you run it". Revisit when
  someone asks for a double-click installer.
  **That trigger fired — bundling is superseded by
  [0019](0019-the-notifier-ships-an-unsigned-installer.md).** The rest of this
  record still stands: the notifier is still outside the workspace, and it is
  still unsigned, for the reasons above.
