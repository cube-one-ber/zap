#!/usr/bin/env bash
# Check that two zap builds print identical output and exit codes for every
# command in a case file. Usage: compare.sh BASELINE CANDIDATE [CASES]
set -uo pipefail
base=$1 cand=$2 cases=${3:-$(dirname "$0")/cases.txt}
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
fail=0
while IFS= read -r line; do
  [[ -z $line || $line == \#* ]] && continue
  eval "set -- $line"
  for width in 60 120; do
    COLUMNS=$width "$base" "$@" >"$out/base" 2>&1; echo "exit $?" >>"$out/base"
    COLUMNS=$width "$cand" "$@" >"$out/cand" 2>&1; echo "exit $?" >>"$out/cand"
    if ! cmp -s "$out/base" "$out/cand"; then
      echo "DIFF [COLUMNS=$width]: $line"
      diff -u "$out/base" "$out/cand" | head -20
      fail=1
    fi
  done
done <"$cases"
[[ $fail == 0 ]] && echo "all equal"
exit $fail
