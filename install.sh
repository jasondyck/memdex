#!/bin/sh
# Install memdex from GitHub releases (Linux x86_64 / aarch64, static binary).
#
#   curl -fsSL https://raw.githubusercontent.com/jasondyck/memdex/main/install.sh | sh
#
# Options (environment variables):
#   MEMDEX_VERSION=v0.4.1    install a specific release instead of the latest
#   MEMDEX_BIN_DIR=DIR       install location (default ~/.local/bin)
#   MEMDEX_REPO=owner/name   install from a fork
#   MEMDEX_BASE_URL=URL      download from a mirror (expects <URL>/<asset>)
set -eu

REPO="${MEMDEX_REPO:-jasondyck/memdex}"
VERSION="${MEMDEX_VERSION:-latest}"
BIN_DIR="${MEMDEX_BIN_DIR:-$HOME/.local/bin}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64 | Linux-amd64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  *) echo "memdex: no prebuilt binary for $(uname -sm); build from source (see README)" >&2; exit 1 ;;
esac

if [ -n "${MEMDEX_BASE_URL:-}" ]; then
  base="$MEMDEX_BASE_URL"
elif [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

for tool in curl tar sha256sum install; do
  command -v "$tool" >/dev/null 2>&1 || { echo "memdex: needs $tool" >&2; exit 1; }
done

asset="memdex-$target.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading $asset ($VERSION)…"
curl -fsSL --proto '=https,http' -o "$tmp/$asset" "$base/$asset"
curl -fsSL --proto '=https,http' -o "$tmp/$asset.sha256" "$base/$asset.sha256"
(cd "$tmp" && sha256sum -c "$asset.sha256") || { echo "memdex: checksum mismatch, not installing" >&2; exit 1; }

tar -C "$tmp" -xzf "$tmp/$asset"
install -Dm755 "$tmp/memdex-$target/memdex" "$BIN_DIR/memdex"
echo "Installed $("$BIN_DIR/memdex" --version) to $BIN_DIR/memdex"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "Note: $BIN_DIR is not on your PATH." ;;
esac

echo
echo "Next: register it with Claude Code (user scope):"
echo "  claude mcp add --scope user memdex -- $BIN_DIR/memdex serve"
