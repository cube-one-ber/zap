#!/usr/bin/env bash
set -euo pipefail
project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
base=$(sed -n 's/^pkgver=//p' "$project_dir/packaging/PKGBUILD.in")
# The compiled-in default must name the same release as the package recipe.
if ! grep -qxF "version = \"$base\"" "$project_dir/Cargo.toml"; then
  printf 'Cargo.toml version does not match pkgver=%s in PKGBUILD.in\n' "$base" >&2
  exit 1
fi
pkgver=${ZAP_PKGVER:-$base}
if [[ ! $pkgver =~ ^[0-9][[:alnum:]_.+]*$ ]]; then
  printf 'Invalid package version: %s\n' "$pkgver" >&2
  exit 1
fi
archive="$project_dir/packaging/zap-source.tar.gz"
tar --sort=name --mtime="@${SOURCE_DATE_EPOCH:-0}" --owner=0 --group=0 --numeric-owner \
  --transform='s,^,zap/,' -C "$project_dir" -cf - \
  --exclude='*.zig' Cargo.toml Cargo.lock build.rs src README.md LICENSE packaging/zap.1 packaging/completions |
  gzip -n > "$archive"
digest=$(sha256sum -- "$archive")
digest=${digest%% *}
sed -e "s/@SOURCE_SHA256@/$digest/" -e "s/^pkgver=.*/pkgver=$pkgver/" \
  "$project_dir/packaging/PKGBUILD.in" > "$project_dir/packaging/PKGBUILD"
printf 'Generated %s and packaging/PKGBUILD\nSHA-256 %s\n' "$archive" "$digest"
