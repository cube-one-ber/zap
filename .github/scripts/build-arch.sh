#!/usr/bin/env bash
set -euo pipefail

# Run in the disposable Arch container; compilation and tests run as the host UID.
if [[ $(id -u) != 0 || ! ${BUILD_UID:-} =~ ^[1-9][0-9]*$ ]]; then
  printf 'Run this build in the CI container with a non-root BUILD_UID.\n' >&2
  exit 1
fi
pacman -Syu --noconfirm curl git pkgconf systemd polkit bubblewrap rust python
useradd -m -u "$BUILD_UID" builder
chown -R builder:builder /workspace

# Check fresh proc/device mounts too, as used by zap's package sandbox.
runuser -u builder -- bwrap --unshare-all --share-net --ro-bind / / --proc /proc --dev /dev -- /usr/bin/true
# Both binaries must build without compiler or clippy warnings. Only CI denies
# them: a new compiler's lints should not break the published PKGBUILD.
runuser -u builder -- cargo clippy --release --locked --bin zap --tests -- -D warnings
runuser -u builder -- cargo clippy --release --locked --features pactest --bin zap-pactest -- -D warnings
# makepkg runs build() and check() without container-root privileges.
runuser -u builder -- bash -c './packaging/source.sh && cd packaging && makepkg --noconfirm'

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
