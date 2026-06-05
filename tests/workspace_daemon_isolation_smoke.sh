#!/usr/bin/env bash
# tests/workspace_daemon_isolation_smoke.sh
#
# The global daemon socket must not collapse typed scratch/candidate writes
# into whichever workspace started graftd first.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
WS_A="$WORKDIR/ws-a"
WS_B="$WORKDIR/ws-b"
trap cleanup_workspace EXIT
require_local_socket_bind

mkdir -p "$WS_A" "$WS_B"
"$GRAFT" --cwd "$WS_A" init >/dev/null
"$GRAFT" --cwd "$WS_B" init >/dev/null

scratch_a=$("$GRAFT" --cwd "$WS_A" scratch write --base graft:empty a.txt --content $'from-a\n')
scratch_a_id=$(grep -oE 'scratch:[0-9a-f]+' <<<"$scratch_a" | tail -n1)
[[ -n $scratch_a_id ]] || { echo "FAIL: workspace A scratch missing"; echo "$scratch_a"; exit 1; }

candidate_a=$("$GRAFT" --cwd "$WS_A" candidate from-scratch "$scratch_a_id" --message workspace-a)
candidate_a_id=$(grep -oE 'candidate:[0-9a-f]+' <<<"$candidate_a" | head -n1)
[[ -n $candidate_a_id ]] || { echo "FAIL: workspace A candidate missing"; echo "$candidate_a"; exit 1; }
[[ -e "$WS_A/.graft/store/private/candidate/$candidate_a_id.json" ]] || {
  echo "FAIL: workspace A candidate was not written to workspace A"; exit 1;
}

scratch_b=$("$GRAFT" --cwd "$WS_B" scratch write --base graft:empty b.txt --content $'from-b\n')
scratch_b_id=$(grep -oE 'scratch:[0-9a-f]+' <<<"$scratch_b" | tail -n1)
[[ -n $scratch_b_id ]] || { echo "FAIL: workspace B scratch missing"; echo "$scratch_b"; exit 1; }

candidate_b=$("$GRAFT" --cwd "$WS_B" candidate from-scratch "$scratch_b_id" --message workspace-b)
candidate_b_id=$(grep -oE 'candidate:[0-9a-f]+' <<<"$candidate_b" | head -n1)
[[ -n $candidate_b_id ]] || { echo "FAIL: workspace B candidate missing"; echo "$candidate_b"; exit 1; }

[[ -e "$WS_B/.graft/store/private/candidate/$candidate_b_id.json" ]] || {
  echo "FAIL: workspace B candidate was not written to workspace B"; exit 1;
}
[[ ! -e "$WS_A/.graft/store/private/candidate/$candidate_b_id.json" ]] || {
  echo "FAIL: workspace B candidate leaked into workspace A"; exit 1;
}

echo "OK: global daemon routes typed scratch/candidate writes by workspace."
