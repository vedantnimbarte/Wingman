# Packaging

Wingman is Windows-first: it's developed and tested on Windows, CI runs the
Windows target, and the release workflow publishes a Windows binary. This doc
covers distributing it via **winget** (and the MSI option).

## Release assets

`.github/workflows/release.yml` fires on a `v*` tag and attaches, per platform:

```
wingman-x86_64-pc-windows-msvc.zip        # contains wingman.exe
wingman-x86_64-pc-windows-msvc.zip.sha256 # checksum sidecar
wingman-aarch64-apple-darwin.tar.gz
wingman-x86_64-unknown-linux-gnu.tar.gz
wingman-aarch64-unknown-linux-gnu.tar.gz
```

## winget (Windows Package Manager)

Wingman ships winget manifest **templates** under `packaging/winget/`. They use
the `zip` installer type with a `portable` nested binary, so no MSI is required —
winget unpacks the release zip and puts `wingman.exe` on the user's PATH as the
`wingman` command.

To publish a version:

```powershell
# 1. Stamp the templates with the version + release checksum.
./scripts/stamp-winget.ps1 -Version 0.1.0
#    → dist/winget/0.1.0/VedantNimbarte.Wingman*.yaml

# 2. Validate locally (requires winget on Windows 10/11).
winget validate --manifest dist/winget/0.1.0

# 3. (Optional) test the install end-to-end in a sandbox.
winget install --manifest dist/winget/0.1.0

# 4. Submit to the community repo. Fork microsoft/winget-pkgs and copy the
#    stamped folder to:
#    manifests/v/VedantNimbarte/Wingman/0.1.0/
#    then open a PR. Or use `wingman-create` / `komac` to automate the PR.
```

After the manifest is merged, users install with:

```powershell
winget install VedantNimbarte.Wingman
```

The `PackageIdentifier` (`VedantNimbarte.Wingman`) must match your winget
publisher identity — adjust the three template files and this doc if you publish
under a different account.

## MSI (optional)

If you'd rather ship a classic installer, `cargo-wix` produces an MSI from the
release build:

```powershell
cargo install cargo-wix
cargo wix init          # once: scaffolds wix/main.wxs
cargo wix -p wingman-cli
```

The MSI can then be added as a release asset and referenced from a winget
manifest with `InstallerType: wix` instead of the `zip`/`portable` combo above.
The zip/portable path is preferred because it reuses the binary the release
workflow already builds and needs no extra toolchain.

## macOS / Linux

The `curl | sh` installer (`scripts/install.sh`) downloads the matching archive
and drops `wingman` on PATH — no package manager needed. Homebrew and a Linux
package (deb/rpm) are future work.
