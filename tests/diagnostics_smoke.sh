#!/usr/bin/env bash
# tests/diagnostics_smoke.sh
#
# Verifies that user-facing diagnostics surface as `[CODE] ... — fix — see:`
# instead of leaking raw upstream errors. Backs the @diagnostic-enum-and-codes
# task evidence requirement.

set -euo pipefail

cd "$(dirname "$0")/.."

cargo build -p graft-cli -p graft-daemon >/dev/null 2>&1
GRAFT_BIN="$PWD/target/debug/graft"
GRAFTD_BIN="$PWD/target/debug/graftd"

WORKDIR="$(mktemp -d)"
cleanup() {
  find "$WORKDIR" -path '*/.graft/run/daemon.sock' -type s -exec "$GRAFTD_BIN" stop --socket {} \; >/dev/null 2>&1 || true
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

cd "$WORKDIR"
echo "smoke" > hello.txt

"$GRAFT_BIN" init >/dev/null

# 1) `create` with no git base in the workspace must fail with a structured
#    diagnostic (B001) instead of leaking raw `fatal: not a git repository`
#    stderr from gix/git.
if "$GRAFT_BIN" create --expect ValidPatch --message b001-smoke >/tmp/create-ok 2>/tmp/create-err; then
  echo "FAIL: create unexpectedly succeeded in a no-git workdir"
  cat /tmp/create-ok
  exit 1
fi
if ! grep -qE '\[B001\] cannot resolve git base `HEAD`' /tmp/create-err; then
  echo "FAIL: B001 not surfaced for unresolved git base"
  cat /tmp/create-err
  exit 1
fi
if grep -qE '^Caused by:' /tmp/create-err; then
  echo "FAIL: raw cause chain (git stderr) leaked under B001"
  cat /tmp/create-err
  exit 1
fi
if grep -q 'graft:empty' /tmp/create-err; then
  :
else
  echo "FAIL: B001 fix hint did not mention --from graft:empty"
  cat /tmp/create-err
  exit 1
fi

# 2) `--from graft:empty` produces a real candidate whose ValidPatch evidence
#    passes, even with no git context.
out=$("$GRAFT_BIN" create --from graft:empty --expect ValidPatch --validate --message empty-base-smoke)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$out" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no candidate id captured for graft:empty base"; echo "$out"; exit 1; }
if ! grep -qE 'evidence: 1 passed,' <<<"$out"; then
  echo "FAIL: ValidPatch did not pass against graft:empty base"
  echo "$out"
  exit 1
fi

# 3) Admit without passing evidence must surface A001/A002 diagnostics.
out2=$("$GRAFT_BIN" create --from graft:empty --expect ValidPatch --message admit-smoke)
candidate2=$(grep -oE 'candidate:[0-9a-f]+' <<<"$out2" | head -n1)
[[ -n $candidate2 ]] || { echo "FAIL: no candidate captured for admit smoke"; echo "$out2"; exit 1; }
if "$GRAFT_BIN" admit "$candidate2" --require ValidPatch >/tmp/admit-ok 2>/tmp/admit-err; then
  echo "FAIL: admit unexpectedly succeeded"
  cat /tmp/admit-ok
  exit 1
fi
if ! grep -qE '\[A00[12]\]' /tmp/admit-err; then
  echo "FAIL: admit failure did not surface an [A00x] diagnostic"
  cat /tmp/admit-err
  exit 1
fi

echo "OK: B001/A00x surface as graft-explain diagnostics with code, fix, and see-also."
