#!/usr/bin/env sh
# Installs the prebuilt `crusty` binary from the GitHub releases of T-Celor/crusty-claude.
#
#   sh scripts/install.sh            # latest release into ~/.local/bin
#   sh scripts/install.sh v0.2.0     # a given release
#   CRUSTY_INSTALL_DIR=/usr/local/bin sh scripts/install.sh
#
# The archive's SHA-256 is checked against the published checksum before anything is installed.
set -eu

REPO="T-Celor/crusty-claude"
VERSION="${1:-latest}"
DEST="${CRUSTY_INSTALL_DIR:-$HOME/.local/bin}"

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
  Linux-x86_64) target="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64 | Linux-arm64) target="aarch64-unknown-linux-gnu" ;;
  Darwin-arm64) target="aarch64-apple-darwin" ;;
  Darwin-x86_64) target="x86_64-apple-darwin" ;;
  *) echo "no prebuilt binary for $os $arch; build from source with: cargo install --path crates/crusty-cli" >&2; exit 1 ;;
esac

if [ "$VERSION" = "latest" ]; then
  VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
  [ -n "$VERSION" ] || { echo "could not find the latest release" >&2; exit 1; }
fi

name="crusty-$VERSION-$target"
base="https://github.com/$REPO/releases/download/$VERSION"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $name.tar.gz"
curl -fsSL -o "$tmp/$name.tar.gz" "$base/$name.tar.gz"
curl -fsSL -o "$tmp/$name.tar.gz.sha256" "$base/$name.tar.gz.sha256"

expected=$(cut -d ' ' -f 1 "$tmp/$name.tar.gz.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$name.tar.gz" | cut -d ' ' -f 1)
else
  actual=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d ' ' -f 1)
fi
[ "$expected" = "$actual" ] || { echo "checksum mismatch: expected $expected, got $actual" >&2; exit 1; }

tar -C "$tmp" -xzf "$tmp/$name.tar.gz"
mkdir -p "$DEST"
install -m 0755 "$tmp/$name/crusty" "$DEST/crusty"
echo "Installed $DEST/crusty ($VERSION)"
case ":$PATH:" in
  *":$DEST:"*) ;;
  *) echo "Add $DEST to your PATH to run 'crusty'." ;;
esac
