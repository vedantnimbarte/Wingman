#!/bin/sh
# wingman installer — downloads a prebuilt `wingman` binary from the latest
# GitHub Release and installs it onto your PATH. No clone, no cargo, no build.
#
#   curl -fsSL https://raw.githubusercontent.com/vedantnimbarte/Wingman/main/scripts/install.sh | sh
#
# Environment overrides:
#   WINGMAN_INSTALL_DIR   where to put the binary   (default: $HOME/.local/bin)
#   VERSION               pin a release tag         (default: latest, e.g. v0.1.0)
#   DRY_RUN=1             print the resolved target + URL and exit (no download)
set -eu

REPO="vedantnimbarte/Wingman"
BIN="wingman"
INSTALL_DIR="${WINGMAN_INSTALL_DIR:-$HOME/.local/bin}"

say()  { printf '%s\n' "$*"; }
err()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- detect platform -> Rust target triple -------------------------------
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Linux)  os_part="unknown-linux-gnu" ;;
  Darwin) os_part="apple-darwin" ;;
  *) err "unsupported OS '$os'. On Windows use scripts/install.ps1 instead." ;;
esac

case "$arch" in
  x86_64 | amd64)          arch_part="x86_64" ;;
  aarch64 | arm64)         arch_part="aarch64" ;;
  *) err "unsupported architecture '$arch'." ;;
esac

target="${arch_part}-${os_part}"

# No prebuilt binary for Intel macOS — GitHub's Intel runners are being
# retired, so we don't publish that asset. Point those users at cargo.
if [ "$os_part" = "apple-darwin" ] && [ "$arch_part" = "x86_64" ]; then
  err "no prebuilt binary for Intel macOS. Build from source instead:
  cargo install --git https://github.com/${REPO} wingman-cli"
fi

asset="${BIN}-${target}.tar.gz"

if [ -n "${VERSION:-}" ]; then
  url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
else
  url="https://github.com/${REPO}/releases/latest/download/${asset}"
fi

if [ "${DRY_RUN:-0}" = "1" ]; then
  say "target : $target"
  say "asset  : $asset"
  say "url    : $url"
  say "dest   : $INSTALL_DIR/$BIN"
  exit 0
fi

# --- download + extract --------------------------------------------------
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  err "need curl or wget to download the release."
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "Downloading $asset ..."
fetch "$url" "$tmp/$asset" || err "download failed. Has a release been published for $REPO? URL: $url"
tar -xzf "$tmp/$asset" -C "$tmp" || err "failed to extract $asset."

# The archive contains the bare binary at its root.
if [ ! -f "$tmp/$BIN" ]; then
  found="$(find "$tmp" -name "$BIN" -type f | head -n1)"
  [ -n "$found" ] || err "'$BIN' not found inside the archive."
  mv "$found" "$tmp/$BIN"
fi

mkdir -p "$INSTALL_DIR"
mv "$tmp/$BIN" "$INSTALL_DIR/$BIN"
chmod +x "$INSTALL_DIR/$BIN"

say "Installed $BIN to $INSTALL_DIR/$BIN"

# --- PATH guidance -------------------------------------------------------
case ":$PATH:" in
  *":$INSTALL_DIR:"*) say "Run: $BIN --help" ;;
  *)
    say ""
    say "$INSTALL_DIR is not on your PATH. Add it, then restart your shell:"
    say "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.bashrc   # or ~/.zshrc"
    say "Or run it directly: $INSTALL_DIR/$BIN --help"
    ;;
esac
