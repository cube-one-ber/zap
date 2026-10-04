#!/usr/bin/env bash
set -euo pipefail
project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
archive="$project_dir/packaging/zap-source.tar.gz"
tar --sort=name --mtime="@${SOURCE_DATE_EPOCH:-0}" --owner=0 --group=0 --numeric-owner \
  --transform='s,^,zap/,' -C "$project_dir" -cf - \
  build.zig src README.md LICENSE packaging/zap.1 packaging/completions |
  gzip -n > "$archive"
digest=$(sha256sum -- "$archive")
digest=${digest%% *}
sed "s/@SOURCE_SHA256@/$digest/" "$project_dir/packaging/PKGBUILD.in" > "$project_dir/packaging/PKGBUILD"
printf 'Generated %s and packaging/PKGBUILD\nSHA-256 %s\n' "$archive" "$digest"
