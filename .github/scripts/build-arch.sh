#!/usr/bin/env bash
set -euo pipefail

# Run in the disposable Arch container; compilation and tests run as the host UID.
if [[ $(id -u) != 0 || ! ${BUILD_UID:-} =~ ^[1-9][0-9]*$ ]]; then
  printf 'Run this build in the CI container with a non-root BUILD_UID.\n' >&2
  exit 1
fi
pacman -Syu --noconfirm curl git pkgconf systemd polkit bubblewrap rust python
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
runuser -u builder -- bash -c './packaging/source.sh && cd packaging && makepkg --noconfirm'

# The package must match the Zig baseline's output and must not be slower.
runuser -u builder -- zig build -Doptimize=ReleaseSafe -Dversion="$ZAP_PKGVER"
runuser -u builder -- bench/compare.sh zig-out/bin/zap packaging/pkg/zap/usr/bin/zap
runuser -u builder -- python3 bench/bench.py zig-out/bin/zap packaging/pkg/zap/usr/bin/zap --min-speedup 0.8
# The engine must keep passing pacman's own transaction tests.
runuser -u builder -- git clone --quiet https://gitlab.archlinux.org/pacman/pacman.git /tmp/pacman
runuser -u builder -- git -C /tmp/pacman checkout --quiet 54d94116
runuser -u builder -- cargo build --release --locked --features pactest --bin zap-pactest
# Container root runs it directly; each test still gets its own temporary root.
python3 pactest/run.py --pacman-src /tmp/pacman --bin target/release/zap-pactest --expect pactest/expected.txt -j "$(nproc)"

mkdir -p dist
cp packaging/*.pkg.tar.zst packaging/zap-source.tar.gz packaging/PKGBUILD dist/
printf 'version=%s\ncommit=%s\narchitecture=x86_64\nrust=%s\n' "$ZAP_PKGVER" "$GITHUB_SHA" "$(rustc -V | cut -d' ' -f2)" > dist/build-info.txt
cd dist
sha256sum -- *.pkg.tar.zst zap-source.tar.gz PKGBUILD build-info.txt > SHA256SUMS
sha256sum -c SHA256SUMS
