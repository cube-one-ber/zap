#!/usr/bin/env bash
# Regenerate the AUR recipe for a published GitHub release: PKGBUILD from
# ../PKGBUILD.in, pinned to that release's source archive, then .SRCINFO.
# Usage: update.sh VERSION   (the release tag without its leading v)
set -euo pipefail
version=${1:?usage: update.sh VERSION}
dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
url="https://github.com/cube-one-ber/zap/releases/download/v$version/zap-source.tar.gz"
archive=$(mktemp)
trap 'rm -f "$archive"' EXIT
curl -fsSL --proto '=https' --tlsv1.2 -o "$archive" "$url"
digest=$(sha256sum -- "$archive")
digest=${digest%% *}
{
  echo '# Maintainer: Cube1ber (https://github.com/cube-one-ber)'
  sed -e '/^# Generate packaging\/PKGBUILD/d' \
    -e "s/^pkgver=.*/pkgver=$version/" \
    -e 's|^source=.*|source=("zap-$pkgver.tar.gz::$url/releases/download/v$pkgver/zap-source.tar.gz")|' \
    -e "s/@SOURCE_SHA256@/$digest/" \
    -e "/^makedepends=/a conflicts=('zap-git' 'zap-bin')" \
    "$dir/../PKGBUILD.in"
} > "$dir/PKGBUILD"
(cd "$dir" && makepkg --printsrcinfo > .SRCINFO)
printf 'Generated PKGBUILD and .SRCINFO for %s\nSHA-256 %s\n' "$version" "$digest"
