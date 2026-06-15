#!/usr/bin/env bash
# tests/hole_report_smoke.sh
#
# Verifies that command output now ends with a labeled Hole Report block
# instead of the legacy single-line `next:` text, and that --json carries
# kind/why fields per next_action. Backs the @hole-report-and-next-actions
# task evidence requirements.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
trap cleanup_workspace EXIT

cd "$WORKDIR"
echo "smoke" > hello.txt

"$GRAFT_BIN" init >/dev/null
scratch_out=$("$GRAFT_BIN" scratch write --base graft:empty hello.txt --content $'smoke\n')
scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$scratch_out" | tail -n1)
[[ -n $scratch ]] || { echo "FAIL: no scratch id captured"; echo "$scratch_out"; exit 1; }
candidate_out=$("$GRAFT_BIN" patch from-scratch "$scratch" --message t5-smoke)
candidate=$(first_graft_id candidate "$candidate_out")
[[ -n $candidate ]] || { echo "FAIL: no candidate id captured"; echo "$candidate_out"; exit 1; }

out=$("$GRAFT_BIN" patch validate "$candidate")

# 1) human output must end with a Hole Report block (header `next:` + at
#    least one `[kind] command` row).
if ! grep -qE '^next:$' <<<"$out"; then
  echo "FAIL: validate human output missing Hole Report header"
  echo "----"; echo "$out"; exit 1
fi
if ! grep -qE '^[[:space:]]+\[recommended\] graft patch admit ' <<<"$out"; then
  echo "FAIL: validate Hole Report missing [recommended] patch admit action"
  echo "----"; echo "$out"; exit 1
fi
if grep -qE '^next: graft ' <<<"$out"; then
  echo "FAIL: legacy single-line 'next: <cmd>' format leaked through"
  echo "----"; echo "$out"; exit 1
fi

# 2) JSON output must carry next_actions[] with structured kind/why fields.
json=$("$GRAFT_BIN" --json patch validate "$candidate")
if ! grep -qE '"kind": "recommended"' <<<"$json"; then
  echo "FAIL: --json validate missing recommended next_action kind"
  echo "$json"; exit 1
fi
if ! grep -qE '"why":' <<<"$json"; then
  echo "FAIL: --json validate missing why fields on next_actions"
  echo "$json"; exit 1
fi

# 3) The --json next_actions on validate must include human-readable why text
#    referencing the recommended next step.
validate=$("$GRAFT_BIN" patch validate "$candidate")
if ! grep -qE '\[recommended\]' <<<"$validate"; then
  echo "FAIL: validate output missing [recommended] action"
  echo "----"; echo "$validate"; exit 1
fi
if ! grep -qE '^[[:space:]]+no constraint evidence is required' <<<"$validate"; then
  echo "FAIL: validate why text missing for core-integrity-only stage"
  echo "----"; echo "$validate"; exit 1
fi

echo "OK: Hole Report block renders for human output; --json carries kind/why."
