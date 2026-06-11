#!/usr/bin/env bash
# tests/scratch_cli_smoke.sh
#
# Verifies graft scratch CLI path with daemon auto-spawn. Scratch commands use
# the global $GRAFT_HOME/run/daemon.sock by default, so the CLI no longer needs
# a manually started daemon.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
PROJECT="$WORKDIR/project"
trap cleanup_workspace EXIT
mkdir -p "$PROJECT"
require_local_socket_bind
printf 'hello\n' > "$PROJECT/hello.txt"
mkdir -p "$PROJECT/nested"

"$GRAFT" --cwd "$PROJECT" init >/dev/null
seed_write=$("$GRAFT" --cwd "$PROJECT/nested" scratch write --base graft:empty hello.txt --content $'hello\n')
seed_scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$seed_write" | tail -n1)
[[ -n $seed_scratch ]] || { echo "FAIL: no seed scratch"; echo "$seed_write"; exit 1; }
create=$("$GRAFT" --cwd "$PROJECT/nested" candidate from-scratch "$seed_scratch" --message scratch-base)
candidate=$(first_graft_id candidate "$create")
[[ -n $candidate ]] || { echo "FAIL: no base candidate"; echo "$create"; exit 1; }
"$GRAFTD" start --cwd "$PROJECT" --socket "$SOCKET"
[[ -S "$SOCKET" ]] || { echo "FAIL: explicit global graftd start did not create socket"; exit 1; }
status=$("$GRAFT" --cwd "$PROJECT" scratch status)
grep -q '"daemon": "graftd"' <<<"$status" || { echo "FAIL: bad status"; echo "$status"; exit 1; }
workspace_status=$("$GRAFT" --cwd "$PROJECT" workspace status)
grep -q $'daemon_state\tlive' <<<"$workspace_status" || {
  echo "FAIL: workspace status did not report live daemon"; echo "$workspace_status"; exit 1;
}
NO_WORKSPACE="$WORKDIR/no-workspace"
mkdir -p "$NO_WORKSPACE"
status_no_workspace=$("$GRAFT" --cwd "$NO_WORKSPACE" scratch status)
grep -q '"daemon": "graftd"' <<<"$status_no_workspace" || {
  echo "FAIL: scratch status should not require workspace discovery"; echo "$status_no_workspace"; exit 1;
}

read_out=$("$GRAFT" --cwd "$PROJECT" scratch read --base "$candidate" hello.txt --mode text)
grep -q 'hello' <<<"$read_out" || { echo "FAIL: scratch read missing content"; echo "$read_out"; exit 1; }

write=$("$GRAFT" --cwd "$PROJECT" scratch write --base "$candidate" bye.txt --content $'bye\n')
scratch2=$(grep -oE 'scratch:[0-9a-f]+' <<<"$write" | tail -n1)
[[ -n $scratch2 ]] || { echo "FAIL: no scratch id after write"; echo "$write"; exit 1; }

edit=$("$GRAFT" --cwd "$PROJECT" scratch edit --from "$scratch2" bye.txt --edits '[{"kind":"replace_text","old_text":"bye","new_text":"ciao"}]')
scratch3=$(grep -oE 'scratch:[0-9a-f]+' <<<"$edit" | tail -n1)
[[ -n $scratch3 ]] || { echo "FAIL: no scratch id after edit"; echo "$edit"; exit 1; }

read_edit=$("$GRAFT" --cwd "$PROJECT" scratch read --from "$scratch3" bye.txt --mode text)
grep -q 'ciao' <<<"$read_edit" || { echo "FAIL: scratch read after edit missing content"; echo "$read_edit"; exit 1; }

delete=$("$GRAFT" --cwd "$PROJECT" scratch delete --from "$scratch3" bye.txt)
scratch4=$(grep -oE 'scratch:[0-9a-f]+' <<<"$delete" | tail -n1)
[[ -n $scratch4 ]] || { echo "FAIL: no scratch id after delete"; echo "$delete"; exit 1; }

diff=$("$GRAFT" --cwd "$PROJECT" scratch diff "$scratch3" "$scratch4")
grep -q 'bye.txt' <<<"$diff" || { echo "FAIL: scratch diff missing changed path"; echo "$diff"; exit 1; }

pin=$("$GRAFT" --cwd "$PROJECT" scratch pin "$scratch4")
lease=$(first_lease_id "$pin")
[[ -n $lease ]] || { echo "FAIL: no lease"; echo "$pin"; exit 1; }
if "$GRAFT" --cwd "$PROJECT" scratch drop "$scratch4" >/tmp/graft-scratch-drop-pinned.out 2>&1; then
  echo "FAIL: dropped pinned scratch"; exit 1
fi
grep -q 'E_SCRATCH_PINNED' /tmp/graft-scratch-drop-pinned.out || {
  echo "FAIL: pinned drop did not return E_SCRATCH_PINNED"; cat /tmp/graft-scratch-drop-pinned.out; exit 1;
}
"$GRAFT" --cwd "$PROJECT" scratch unpin "$lease" >/dev/null

candidate_out=$("$GRAFT" --cwd "$PROJECT" candidate from-scratch "$scratch4" --message scratch-cli-final)
final_candidate=$(first_graft_id candidate "$candidate_out")
[[ -n $final_candidate ]] || { echo "FAIL: no final candidate from scratch"; echo "$candidate_out"; exit 1; }

"$GRAFTD" stop --socket "$SOCKET" >/dev/null || true

echo "OK: scratch CLI daemon auto-spawn smoke works."
