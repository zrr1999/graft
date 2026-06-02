#!/usr/bin/env bash
# tests/scratch_cli_smoke.sh
#
# Verifies graft scratch CLI path with daemon auto-spawn. Scratch commands use
# the per-workspace .graft/run/daemon.sock by default, so the CLI no longer needs
# a manually started daemon.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/socket_probe.sh

cargo build -p graft-cli -p graft-daemon >/dev/null 2>&1
GRAFT="$PWD/target/debug/graft"
GRAFTD="$PWD/target/debug/graftd"

WORKDIR="$(mktemp -d)"
PROJECT="$WORKDIR/project"
SOCKET="$PROJECT/.graft/run/daemon.sock"
trap 'if [[ -S "$SOCKET" ]]; then "$GRAFTD" stop --socket "$SOCKET" >/dev/null 2>&1 || true; fi; rm -rf "$WORKDIR"' EXIT
mkdir -p "$PROJECT"
skip_if_local_socket_bind_unavailable "$WORKDIR/probe.sock"
printf 'hello\n' > "$PROJECT/hello.txt"

"$GRAFT" --cwd "$PROJECT" init >/dev/null
create=$("$GRAFT" --cwd "$PROJECT" create --from graft:empty --expect ValidPatch --message scratch-base)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$create" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no base candidate"; echo "$create"; exit 1; }
[[ -S "$SOCKET" ]] || { echo "FAIL: create did not auto-spawn workspace graftd"; exit 1; }

status=$("$GRAFT" --cwd "$PROJECT" scratch status)
grep -q '"daemon": "graftd"' <<<"$status" || { echo "FAIL: bad status"; echo "$status"; exit 1; }

open=$("$GRAFT" --cwd "$PROJECT" scratch open --base "$candidate")
scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$open" | head -n1)
[[ -n $scratch ]] || { echo "FAIL: no scratch id"; echo "$open"; exit 1; }

read_out=$("$GRAFT" --cwd "$PROJECT" scratch read "$scratch" hello.txt --mode text)
grep -q 'hello' <<<"$read_out" || { echo "FAIL: scratch read missing content"; echo "$read_out"; exit 1; }

write=$("$GRAFT" --cwd "$PROJECT" scratch write "$scratch" bye.txt --content $'bye\n')
scratch2=$(grep -oE 'scratch:[0-9a-f]+' <<<"$write" | tail -n1)
[[ -n $scratch2 ]] || { echo "FAIL: no scratch id after write"; echo "$write"; exit 1; }

diff=$("$GRAFT" --cwd "$PROJECT" scratch diff "$scratch" "$scratch2")
grep -q 'bye.txt' <<<"$diff" || { echo "FAIL: scratch diff missing changed path"; echo "$diff"; exit 1; }

pin=$("$GRAFT" --cwd "$PROJECT" scratch pin "$scratch2")
lease=$(grep -oE 'lease_[0-9a-f]+' <<<"$pin" | head -n1)
[[ -n $lease ]] || { echo "FAIL: no lease"; echo "$pin"; exit 1; }
if "$GRAFT" --cwd "$PROJECT" scratch drop "$scratch2" >/tmp/graft-scratch-drop-pinned.out 2>&1; then
  echo "FAIL: dropped pinned scratch"; exit 1
fi
grep -q 'E_SCRATCH_PINNED' /tmp/graft-scratch-drop-pinned.out || {
  echo "FAIL: pinned drop did not return E_SCRATCH_PINNED"; cat /tmp/graft-scratch-drop-pinned.out; exit 1;
}
"$GRAFT" --cwd "$PROJECT" scratch unpin "$lease" >/dev/null

promote=$("$GRAFT" --cwd "$PROJECT" scratch promote "$scratch2" --expect ValidPatch --producer smoke --message scratch-promote)
grep -q 'candidate:' <<<"$promote" || { echo "FAIL: scratch promote missing candidate"; echo "$promote"; exit 1; }

"$GRAFTD" stop --socket "$SOCKET" >/dev/null || true

echo "OK: scratch CLI daemon auto-spawn smoke works."
