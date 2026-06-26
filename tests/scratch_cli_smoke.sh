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
REPO="$WORKDIR/base-repo"
trap cleanup_workspace EXIT
mkdir -p "$PROJECT" "$REPO"
require_local_socket_bind
printf 'hello\n' > "$PROJECT/hello.txt"
printf 'hello\n' > "$REPO/hello.txt"
mkdir -p "$PROJECT/nested"
git -C "$REPO" init -q
git -C "$REPO" config user.email "graft-smoke@example.invalid"
git -C "$REPO" config user.name "Graft Smoke"
git -C "$REPO" config commit.gpgsign false
git -C "$REPO" add hello.txt
git -C "$REPO" commit -qm base
base_commit=$(git -C "$REPO" rev-parse HEAD)

"$GRAFT" --cwd "$PROJECT" init >/dev/null
"$GRAFT" --cwd "$PROJECT" repo add --default-branch "$base_commit" base "$REPO" >/dev/null
"$GRAFT" --cwd "$PROJECT" repo lock base >/dev/null
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

if env -u GRAFT_BASE_REF "$GRAFT" --cwd "$PROJECT" scratch read hello.txt --mode text >/tmp/graft-scratch-missing-base.out 2>&1; then
  echo "FAIL: scratch read without base/from/GRAFT_BASE_REF unexpectedly succeeded"; exit 1
fi
grep -q 'E_MISSING_BASE' /tmp/graft-scratch-missing-base.out || {
  echo "FAIL: missing implicit base did not mention E_MISSING_BASE"; cat /tmp/graft-scratch-missing-base.out; exit 1;
}

env_read=$(GRAFT_BASE_REF="$candidate" "$GRAFT" --cwd "$PROJECT" scratch read hello.txt --mode text)
grep -q 'hello' <<<"$env_read" || { echo "FAIL: env base candidate read missing content"; echo "$env_read"; exit 1; }

git_hash_read=$(GRAFT_BASE_REF="repo:base@$base_commit" "$GRAFT" --cwd "$PROJECT" --json scratch read hello.txt --mode text)
GIT_HASH_READ="$git_hash_read" python3 - <<'PY' || { echo "FAIL: env base repo git hash read missing provenance"; echo "$git_hash_read"; exit 1; }
import json, os
result = json.loads(os.environ["GIT_HASH_READ"])["result"]
assert result["content"] == "hello\n", result
assert result["base_state"], result
assert result["base_tree"].startswith("tree:"), result
PY

if GRAFT_BASE_REF="repo:missing@main" "$GRAFT" --cwd "$PROJECT" scratch read hello.txt --mode text >/tmp/graft-scratch-invalid-env.out 2>&1; then
  echo "FAIL: invalid GRAFT_BASE_REF unexpectedly succeeded"; exit 1
fi
grep -q 'repo:missing@main' /tmp/graft-scratch-invalid-env.out || {
  echo "FAIL: invalid GRAFT_BASE_REF did not mention the bad ref"; cat /tmp/graft-scratch-invalid-env.out; exit 1;
}

env_open=$(GRAFT_BASE_REF="$candidate" "$GRAFT" --cwd "$PROJECT" --json scratch open)
ENV_OPEN="$env_open" python3 - <<'PY' || { echo "FAIL: env base scratch open missing provenance"; echo "$env_open"; exit 1; }
import json, os
result = json.loads(os.environ["ENV_OPEN"])["result"]
assert result["scratch"].startswith("scratch:"), result
assert result["base_state"], result
assert result["base_tree"].startswith("tree:"), result
PY

override_write=$(GRAFT_BASE_REF="repo:missing@main" "$GRAFT" --cwd "$PROJECT" scratch write --base graft:empty override.txt --content $'override\n')
override_scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$override_write" | tail -n1)
[[ -n $override_scratch ]] || { echo "FAIL: explicit base did not override invalid env base"; echo "$override_write"; exit 1; }

open_out=$("$GRAFT" --cwd "$PROJECT" --json scratch open --base "$candidate")
open_scratch=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["scratch"])' <<<"$open_out")
[[ $open_scratch == scratch:* ]] || { echo "FAIL: scratch open did not return result.scratch"; echo "$open_out"; exit 1; }

read_out=$("$GRAFT" --cwd "$PROJECT" scratch read --base "$candidate" hello.txt --mode text)
grep -q 'hello' <<<"$read_out" || { echo "FAIL: scratch read missing content"; echo "$read_out"; exit 1; }
if LC_ALL=C printf '\377' | "$GRAFT" --cwd "$PROJECT" scratch write --base "$candidate" bad.txt --content-stdin >/tmp/graft-scratch-write-bad-stdin.out 2>&1; then
  echo "FAIL: invalid UTF-8 stdin content unexpectedly succeeded"; exit 1
fi
if grep -qE 'scratch:[0-9a-f]+' /tmp/graft-scratch-write-bad-stdin.out; then
  echo "FAIL: invalid UTF-8 stdin content produced a scratch id"; cat /tmp/graft-scratch-write-bad-stdin.out; exit 1;
fi

write=$(printf 'bye\n' | "$GRAFT" --cwd "$PROJECT" scratch write --base "$candidate" bye.txt --content-stdin)
scratch2=$(grep -oE 'scratch:[0-9a-f]+' <<<"$write" | tail -n1)
[[ -n $scratch2 ]] || { echo "FAIL: no scratch id after stdin write"; echo "$write"; exit 1; }

edit=$(printf '[{"kind":"replace_text","old_text":"bye","new_text":"ciao"}]' | "$GRAFT" --cwd "$PROJECT" scratch edit --from "$scratch2" bye.txt --edits-stdin)
scratch3=$(grep -oE 'scratch:[0-9a-f]+' <<<"$edit" | tail -n1)
[[ -n $scratch3 ]] || { echo "FAIL: no scratch id after edit"; echo "$edit"; exit 1; }
if printf 'not-json' | "$GRAFT" --cwd "$PROJECT" scratch edit --from "$scratch2" bye.txt --edits-stdin >/tmp/graft-scratch-edit-bad-stdin.out 2>&1; then
  echo "FAIL: invalid stdin edits unexpectedly succeeded"; exit 1
fi
if grep -qE 'scratch:[0-9a-f]+' /tmp/graft-scratch-edit-bad-stdin.out; then
  echo "FAIL: invalid stdin edits produced a scratch id"; cat /tmp/graft-scratch-edit-bad-stdin.out; exit 1;
fi
read_after_bad=$("$GRAFT" --cwd "$PROJECT" scratch read --from "$scratch2" bye.txt --mode text)
grep -q 'bye' <<<"$read_after_bad" || { echo "FAIL: invalid stdin edits changed parent scratch"; echo "$read_after_bad"; exit 1; }

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
