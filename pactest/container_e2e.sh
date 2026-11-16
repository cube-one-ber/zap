#!/usr/bin/env bash
# Pre-release check on a real system: installs the built zap package in a
# disposable archlinux:base-devel container, then runs a real `zap upgrade`
# (DB download, DownloadUser sandbox, parallel downloads, keyring, CheckSpace),
# `zap install`, `zap remove` and `zap install-files` with lz4, lzip, .Z,
# lrzip and lzop archives, and checks the result with pacman.
# Needs docker as root. Usage: container_e2e.sh path/to/zap-VERSION.pkg.tar.zst
# Host networking because the docker bridge may lack outbound access; the
# container then uses the host's systemd-resolved stub.
set -euo pipefail
if [[ ${IN_CONTAINER:-} != 1 ]]; then
  pkg=$(realpath "$1")
  resolv=$(mktemp)
  trap 'rm -f "$resolv"' EXIT
  printf 'nameserver 127.0.0.53\noptions edns0 trust-ad\n' > "$resolv"
  chmod 644 "$resolv" # the DownloadUser must read it
  docker run --rm --network host --env IN_CONTAINER=1 \
    --security-opt seccomp=unconfined --security-opt apparmor=unconfined \
    --volume "$resolv:/etc/resolv.conf:ro" --volume "$pkg:/zap.pkg.tar.zst:ro" \
    --volume "$(realpath "$0"):/e2e.sh:ro" archlinux:base-devel bash /e2e.sh
  exit
fi

step() { printf '\n===== %s =====\n' "$*"; }
step setup
pacman-key --init >/dev/null 2>&1
pacman-key --populate archlinux >/dev/null 2>&1
pacman -Sy --noconfirm --needed git bubblewrap gnupg >/dev/null
pacman -U --noconfirm /zap.pkg.tar.zst >/dev/null
useradd -m tester
echo 'tester ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/tester
# zap downloads the sync DBs itself.
rm -f /var/lib/pacman/sync/*
# Real pacman (-U above) leaves an empty download-* dir; only zap's count.
rm -rf /var/cache/pacman/pkg/download-*
# zap wants a terminal. The answers are finite: sudo's pty relay spins
# forever on an endless `yes`.
cat > /ptyrun.py <<'EOF'
import os, pty, sys
status = pty.spawn(["sh", "-c", 'stty -echo; exec "$@"', "sh"] + sys.argv[1:])
sys.exit(os.waitstatus_to_exitcode(status))
EOF
# The image strips NoExtract'd /usr/share docs and has its own GID quirks,
# so -Qkk is checked against this baseline.
qkk() { { pacman -Qkk 2>&1 || true; } | grep '^warning:' | grep -v ' /usr/share/.*(No such file' | sort -u; }
qkk > /qkk.before
z() { su tester -c "cd ~ && printf 'y\n%.0s' {1..20} | python3 /ptyrun.py zap $* --elevate sudo"; }

step "zap upgrade"
z upgrade
[[ -z $(pacman -Qu) ]] || { echo "updates still pending"; exit 1; }

step "zap install"
z install tree lrzip lzop ncompress lzip
tree --version

step "zap remove"
z remove tree
! pacman -Q tree 2>/dev/null || { echo "tree still installed"; exit 1; }

step "zap install-files"
mkdir /tmp/arch && cd /tmp/arch
for c in lz4 lzip Z lrz lzo; do
  mkdir -p $c/usr/share/zaptest
  echo "$c payload" > $c/usr/share/zaptest/$c
  printf 'pkgname = zaptest-%s\npkgver = 1-1\narch = any\nsize = 16\n' $c > $c/.PKGINFO
  bsdtar -C $c -cf $c.tar .PKGINFO usr
done
lz4 -q lz4.tar zaptest-lz4-1-1-any.pkg.tar.lz4
lzip -c lzip.tar > zaptest-lzip-1-1-any.pkg.tar.lz
compress -c Z.tar > zaptest-Z-1-1-any.pkg.tar.Z
lrzip -q -o zaptest-lrz-1-1-any.pkg.tar.lrz lrz.tar
lzop -c lzo.tar > zaptest-lzo-1-1-any.pkg.tar.lzo
chmod 755 /tmp/arch && chmod 644 /tmp/arch/*.pkg.tar.*
z install-files /tmp/arch/*.pkg.tar.*
for c in lz4 lzip Z lrz lzo; do
  [[ $(cat /usr/share/zaptest/$c) == "$c payload" ]] || { echo "zaptest-$c not installed"; exit 1; }
done

step verify
pacman -Dk
# Only the test archives (no mtree) may add -Qkk warnings, and the journal
# group systemd's tmpfiles hook sets on upgrade (real pacman gets it too).
new=$(qkk | comm -13 /qkk.before - | grep -v -e ': zaptest-' -e 'systemd: /var/log/journal (GID mismatch)' || true)
[[ -z $new ]] || { echo "new pacman -Qkk warnings:"; echo "$new"; exit 1; }
! ls -d /var/cache/pacman/pkg/download-* 2>/dev/null || { echo "leftover sandbox dir"; exit 1; }
! ls /var/cache/pacman/pkg/*.part 2>/dev/null || { echo "leftover .part"; exit 1; }
step "all passed"
