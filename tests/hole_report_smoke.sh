#!/usr/bin/env bash
# tests/hole_report_smoke.sh
#
# Verifies that command output now ends with a labeled Hole Report block
# instead of the legacy single-line `next:` text, and that --json carries
# kind/why fields per next_action. Backs the @hole-report-and-next-actions
# task evidence requirements.

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
out=$("$GRAFT_BIN" create --from graft:empty --expect ValidPatch --message t5-smoke)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$out" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no candidate id captured"; exit 1; }

# 1) human output must end with a Hole Report block (header `next:` + at
#    least one `[kind] command` row).
if ! grep -qE '^next:$' <<<"$out"; then
  echo "FAIL: create human output missing Hole Report header"
  echo "----"; echo "$out"; exit 1
fi
if ! grep -qE '^[[:space:]]+\[recommended\] graft validate ' <<<"$out"; then
  echo "FAIL: create Hole Report missing [recommended] action"
  echo "----"; echo "$out"; exit 1
fi
if grep -qE '^next: graft ' <<<"$out"; then
  echo "FAIL: legacy single-line 'next: <cmd>' format leaked through"
  echo "----"; echo "$out"; exit 1
fi

# 2) JSON output must carry next_actions[] with structured kind/why fields.
json=$("$GRAFT_BIN" --json validate "$candidate")
if ! grep -qE '"kind": "recommended"' <<<"$json"; then
  echo "FAIL: --json validate missing recommended next_action kind"
  echo "$json"; exit 1
fi
if ! grep -qE '"why":' <<<"$json"; then
  echo "FAIL: --json validate missing why fields on next_actions"
  echo "$json"; exit 1
fi

# 3) The --json next_actions on validate must include human-readable why text
#    referencing the recommended next step. (Historically this stage was used
#    to assert that V003 surfaced in unknown-only flows; with --from
#    graft:empty as the documented no-git escape hatch, ValidPatch now passes
#    in this fixture, so we just confirm the Hole Report wording stays
#    informative.)
validate=$("$GRAFT_BIN" validate "$candidate")
if ! grep -qE '\[recommended\]' <<<"$validate"; then
  echo "FAIL: validate output missing [recommended] action"
  echo "----"; echo "$validate"; exit 1
fi
if ! grep -qE '^[[:space:]]+candidate has passing evidence' <<<"$validate"; then
  echo "FAIL: validate why text missing for passing-evidence stage"
  echo "----"; echo "$validate"; exit 1
fi

echo "OK: Hole Report block renders for human output; --json carries kind/why."
