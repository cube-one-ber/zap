#!/usr/bin/env bash
# End-to-end run of `zap __transaction` against a throwaway root.
# Re-execs itself in a user + mount namespace and bind-mounts a temporary
# pacman.conf over /etc/pacman.conf there, so the system is never touched.
# Uses the iana-etc and tzdata archives in pacman's cache (neither has
# dependencies). Usage: worker_e2e.sh ZAP_BINARY
set -euo pipefail
zap=$(realpath "$1")
if [[ ${IN_NS:-} != 1 ]]; then
  exec env IN_NS=1 unshare --map-root-user --mount --propagation private "$0" "$@"
fi
T=$(mktemp -d "${TMPDIR:-/tmp}/zap-worker-XXXXXX")
newest() { ls /var/cache/pacman/pkg/$1-[0-9]*.pkg.tar.zst | sort -V | tail -1; }
root=$T/root
mkdir -p "$root"/var/lib/pacman/{local,sync} "$root"/var/cache/pacman/pkg "$root"/var/log "$T/repo" "$root/etc/pacman.d/hooks"
echo 9 > "$root/var/lib/pacman/local/ALPM_DB_VERSION"
cp "$(newest iana-etc)" "$T/repo/"
repo-add -q "$T/repo/test.db.tar.gz" "$T/repo/"*.pkg.tar.zst
cat > "$T/pacman.conf" <<EOF
[options]
RootDir = $root
DBPath = $root/var/lib/pacman/
CacheDir = $root/var/cache/pacman/pkg/
LogFile = $root/var/log/pacman.log
GPGDir = $root/etc/pacman.d/gnupg/
HookDir = $root/etc/pacman.d/hooks/
HoldPkg = tzdata
Architecture = auto
SigLevel = Never
[test]
Server = file://$T/repo
EOF
cat > "$root/etc/pacman.d/hooks/note.hook" <<EOF
[Trigger]
Type = Package
Operation = Install
Target = iana-etc
[Action]
Description = Recording the install
When = PostTransaction
Exec = /usr/bin/true
EOF
mount --bind "$T/pacman.conf" /etc/pacman.conf

step() {
  local answer=$1 json=$2
  echo "=== $json (answer: $answer)"
  printf '%s\n' "$answer" | script -qec "$zap __transaction '$json'" /dev/null | sed 's/\x1b\[[0-9;]*[a-zA-Z]//g; s/\r//g' | tail -25 || true
}
tz=$(newest tzdata)
tzsum=$(sha256sum "$tz" | cut -d' ' -f1)

step y '{"operation":"upgrade"}'
step n '{"operation":"install","targets":[{"name":"iana-etc"}]}'
ls "$root/var/lib/pacman/local"
step y '{"operation":"install","targets":[{"name":"iana-etc"},{"name":"tzdata","archive":"'"$tz"'","sha256":"'"$tzsum"'"}]}'
ls "$root/var/lib/pacman/local"; ls "$root/etc" | head; tail -5 "$root/var/log/pacman.log"
step y '{"operation":"reason","targets":[{"name":"iana-etc","reason":"dependency"}]}'
grep -A1 '%REASON%' "$root"/var/lib/pacman/local/iana-etc-*/desc || echo "(no reason line: explicit)"
step y '{"operation":"install","targets":[{"name":"tzdata","archive":"'"$tz"'","sha256":"'"${tzsum/a/b}"'"}]}'
step y '{"operation":"remove","targets":[{"name":"tzdata"}]}'
step y '{"operation":"remove","targets":[{"name":"iana-etc"}]}'
ls "$root/var/lib/pacman/local"; ls "$root/etc"
rm -rf "$T"
