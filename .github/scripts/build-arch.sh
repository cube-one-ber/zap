#!/usr/bin/env bash
set -euo pipefail

# Run in the disposable Arch container; compilation and tests run as the host UID.
if [[ $(id -u) != 0 || ! ${BUILD_UID:-} =~ ^[1-9][0-9]*$ ]]; then
  printf 'Run this build in the CI container with a non-root BUILD_UID.\n' >&2
  exit 1
fi
pacman -Syu --noconfirm curl git pkgconf systemd polkit bubblewrap
curl -fL --proto '=https' --tlsv1.2 https://ziglang.org/download/0.15.2/zig-x86_64-linux-0.15.2.tar.xz -o /tmp/zig.tar.xz
echo '02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239  /tmp/zig.tar.xz' | sha256sum -c -
tar -xf /tmp/zig.tar.xz -C /opt
ln -s /opt/zig-x86_64-linux-0.15.2/zig /usr/local/bin/zig
useradd -m -u "$BUILD_UID" builder
chown -R builder:builder /workspace

runuser -u builder -- zig fmt --check build.zig src
# Check fresh proc/device mounts too, as used by zap's package sandbox.
runuser -u builder -- bwrap --unshare-all --share-net --ro-bind / / --proc /proc --dev /dev -- /usr/bin/true
# makepkg runs build() and check() without container-root privileges.
runuser -u builder -- bash -c './packaging/source.sh && cd packaging && ZIG=/usr/local/bin/zig makepkg --noconfirm'

mkdir -p dist
cp packaging/*.pkg.tar.zst packaging/zap-source.tar.gz packaging/PKGBUILD dist/
printf 'version=%s\ncommit=%s\narchitecture=x86_64\nzig=0.15.2\n' "$ZAP_PKGVER" "$GITHUB_SHA" > dist/build-info.txt
cd dist
sha256sum -- *.pkg.tar.zst zap-source.tar.gz PKGBUILD build-info.txt > SHA256SUMS
sha256sum -c SHA256SUMS
