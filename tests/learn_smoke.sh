#!/usr/bin/env bash
# tests/learn_smoke.sh
#
# Verifies `graft learn --non-interactive` runs the tutorial path through
# init -> create -> validate -> admit -> materialize dry-run -> promote dry-run
# while quoting explain-derived step summaries.

set -euo pipefail

cd "$(dirname "$0")/.."

cargo build -p graft-cli -p graft-daemon >/dev/null 2>&1
GRAFT_BIN="$PWD/target/debug/graft"

out=$("$GRAFT_BIN" learn --non-interactive)

for step in init create validate admit materialize promote; do
  if ! grep -qE "^step: $step$" <<<"$out"; then
    echo "FAIL: learn output missing step $step"
    echo "$out"; exit 1
  fi
  if ! grep -A1 -E "^step: $step$" <<<"$out" | grep -q '^explain: '; then
    echo "FAIL: learn step $step missing explain summary"
    echo "$out"; exit 1
  fi
done

if ! grep -qE 'learn candidate: candidate:[0-9a-f]+' <<<"$out"; then
  echo "FAIL: learn output missing candidate id"
  echo "$out"; exit 1
fi
if ! grep -qE 'learn patch: patch:[0-9a-f]+' <<<"$out"; then
  echo "FAIL: learn output missing patch id"
  echo "$out"; exit 1
fi
if ! grep -q 'learn complete:' <<<"$out"; then
  echo "FAIL: learn output missing completion summary"
  echo "$out"; exit 1
fi
if ! grep -q 'skipped side effects:' <<<"$out"; then
  echo "FAIL: learn output missing side-effect summary"
  echo "$out"; exit 1
fi

echo "OK: learn non-interactive tutorial runs through the compiler-as-docs path."
