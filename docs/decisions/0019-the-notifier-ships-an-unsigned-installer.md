# 0019 — The notifier ships an installer, and it is unsigned

**Status:** accepted
**Date:** 2026-09-05

## Context

[0018](0018-the-notifier-is-not-a-workspace-member.md) deferred bundling with a
trigger: *"Revisit when someone asks for a double-click installer."* Someone
did. This records what that turned into, and what it deliberately did not.

The immediate cost of having no installer turned out to be larger than the
record anticipated. A binary that only exists at
`target/release/wingman-notify.exe` is not an application as far as the
operating system is concerned: it has no Start-menu entry and no registration,
so nothing that resolves applications by name can see it. That is what blocked
the last unverified path in the notification work — the popup's own buttons
could not be driven by anything except a human at the keyboard, because no
automation could name the app to ask for permission to drive it.

## Decision

`bundle.active` is on, with targets `nsis`, `deb`, `appimage`, `dmg` — the
native format for each platform, chosen per-platform by the bundler.

**NSIS rather than MSI on Windows.** MSI is built through WiX, which the CLI
downloads on first use and which wants more of the installer surface described
(upgrade codes, feature trees) than a single-binary tray app has to say. NSIS
produces one `.exe`, needs no extra toolchain, and is what Tauri's own default
is. MSI is worth adding the day someone needs Group Policy deployment; nothing
here does.

**`installMode: currentUser`.** No elevation prompt, installs under the user
profile. A notification popup is a per-user tool; there is no argument for
writing to `Program Files` and every install that asks for admin is one more
reason not to run it.

**The Tauri CLI is pinned in `desktop/notifier/package.json`.** That is a third
npm project in the repo, which is worth a sentence: the CLI has to run from the
directory holding `tauri.conf.json`, and `beforeBuildCommand` already assumes
that same cwd. Putting it in `ui/package.json` would mean pointing the CLI at a
config in its parent and hoping every relative path in that config resolved from
the right place. Dependabot covers it like the other two.

## What this deliberately does not do

**It is not signed.** 0018's arithmetic on signing has not changed: a Windows OV
certificate is weeks of procurement and HSM-backed since June 2023, and macOS
notarization needs a paid developer account. So SmartScreen shows "Windows
protected your PC" on first run and the user has to click through *More info →
Run anyway*. That is an honest cost and it is stated in `NOTIFIER.md` rather
than left to be discovered.

Signing is the thing to revisit when the notifier is offered to anyone who did
not build it themselves. For a tool you install from your own checkout, the
warning is noise; for a download link, it is a wall.

**~~It is not wired into `release.yml`.~~** *That trigger fired after v0.3.0
shipped without an installer while notifications were its headline feature.* The
`notifier-bundle` job now builds one per platform and uploads it beside the CLI
archives.

Two things about that job are deliberate. It is `continue-on-error`, because the
CLI binaries are what `install.sh` serves and a bundler that breaks — a webkit
package renamed, an AppImage tool that will not fetch — must never withhold
them; a failed leg leaves that platform exactly where it was before the job
existed. And the bundle *step* is soft-failing too, because `tauri build` walks
its target list in order: on Linux the `.deb` is already on disk when the
AppImage runs, so a non-zero exit there would throw away a bundle that built
fine. The upload step fails loudly only when nothing at all was produced.

Assets are renamed to `wingman-notify-<target>.<ext>` — Tauri names its output
after the product and version ("Wingman Notify_0.3.0_x64-setup.exe"), which
percent-encodes in a download URL and does not say which target it is for.

**CI does not build the bundle.** The `desktop-notifier` job compiles and tests
the crate, which is what catches the mistakes that matter. Bundling adds minutes
and a toolchain download to an informational job in order to re-prove that a
bundler works.

## Consequences

- `wingman notify` can find an installed notifier the same way it finds a
  sibling binary, and the app is now resolvable by name to anything that
  enumerates installed applications.
- First run shows a SmartScreen warning. Documented, not fixed.
- The bundle output lands under `target/`, which is already ignored.
