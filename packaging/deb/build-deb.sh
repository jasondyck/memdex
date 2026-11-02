#!/bin/bash
# Build a .deb from a (static) memdex binary using only dpkg-deb.
#   packaging/deb/build-deb.sh BINARY VERSION DEB_ARCH OUT_DIR    (DEB_ARCH: amd64 | arm64)
set -euo pipefail
bin=$1 ver=$2 arch=$3 out=$4
root=$(cd "$(dirname "$0")/../.." && pwd)
pkg="$(mktemp -d)/memdex_${ver}_${arch}"
mkdir -p "$pkg/DEBIAN" "$pkg/usr/bin" "$pkg/usr/share/doc/memdex/examples"
install -m755 "$bin" "$pkg/usr/bin/memdex"
install -m644 "$root/README.md" "$pkg/usr/share/doc/memdex/README.md"
cp "$root"/deploy/* "$pkg/usr/share/doc/memdex/examples/"
chmod 644 "$pkg"/usr/share/doc/memdex/examples/*
{
  echo "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/"
  echo "Upstream-Name: memdex"
  echo "Source: https://github.com/jasondyck/memdex"
  echo
  echo "Files: *"
  echo "Copyright: memdex contributors"
  echo "License: MIT or Apache-2.0"
  echo
  echo "License: MIT"
  sed 's/^/ /; s/^ $/ ./' "$root/LICENSE-MIT"
  echo
  echo "License: Apache-2.0"
  echo " On Debian systems, the full text of the Apache License 2.0 can be found in"
  echo " /usr/share/common-licenses/Apache-2.0."
} > "$pkg/usr/share/doc/memdex/copyright"
# Native-package changelog (Debian policy); dated from the binary for reproducibility.
when=$(date -u -R -d "@$(stat -c %Y "$bin")")
printf 'memdex (%s) unstable; urgency=medium\n\n  * Release %s: https://github.com/jasondyck/memdex/releases/tag/v%s\n\n -- memdex contributors <6395637+jasondyck@users.noreply.github.com>  %s\n' \
  "$ver" "$ver" "$ver" "$when" | gzip -9n > "$pkg/usr/share/doc/memdex/changelog.gz"
# The binary is deliberately static (no runtime dependencies on any distro release).
mkdir -p "$pkg/usr/share/lintian/overrides"
printf 'memdex: statically-linked-binary [usr/bin/memdex]\nmemdex: shared-library-lacks-prerequisites [usr/bin/memdex]\n' \
  > "$pkg/usr/share/lintian/overrides/memdex"
size=$(du -sk --exclude=DEBIAN "$pkg" | cut -f1)
cat > "$pkg/DEBIAN/control" <<CTL
Package: memdex
Version: $ver
Architecture: $arch
Maintainer: memdex contributors <6395637+jasondyck@users.noreply.github.com>
Installed-Size: $size
Section: utils
Priority: optional
Homepage: https://github.com/jasondyck/memdex
Description: fast, lightweight shared memory for AI agents over MCP
 Full-text search over markdown memory notes (Claude Code memory dirs plus a
 shared vault any agent can write to), served over MCP (stdio or HTTP).
 Static binary, no dependencies. Register with Claude Code:
   claude mcp add --scope user memdex -- /usr/bin/memdex serve
CTL
mkdir -p "$out"
dpkg-deb --root-owner-group -Zxz --build "$pkg" "$out/memdex_${ver}_${arch}.deb" >/dev/null
echo "$out/memdex_${ver}_${arch}.deb"
