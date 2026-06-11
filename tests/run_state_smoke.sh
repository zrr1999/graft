#!/usr/bin/env bash
# tests/run_state_smoke.sh
#
# Verifies state-first graft run: state refs materialize to a temporary complete
# state root, --cwd is relative to that root, stdout/stderr/exit_code are stable,
# and command writes are discarded.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
trap cleanup_workspace EXIT

cd "$WORKDIR"
"$GRAFT" init >/dev/null

scratch_out=$("$GRAFT" scratch write --base graft:empty worktrees/A/README.md --content $'repo A\n')
scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$scratch_out" | tail -n1)
[[ -n $scratch ]] || { echo "FAIL: scratch write did not return scratch id"; echo "$scratch_out"; exit 1; }

candidate_out=$("$GRAFT" patch from-scratch "$scratch" --message state-run-smoke)
candidate=$(first_graft_id candidate "$candidate_out")
[[ -n $candidate ]] || { echo "FAIL: patch from-scratch did not return candidate id"; echo "$candidate_out"; exit 1; }

admit_out=$("$GRAFT" patch admit "$candidate")
patch=$(first_graft_id patch "$admit_out")
[[ -n $patch ]] || { echo "FAIL: admit did not return patch id"; echo "$admit_out"; exit 1; }

run_json=$("$GRAFT" --json run "$patch" --cwd worktrees/A -- /bin/sh -c 'test -f README.md; printf run-ok; printf run-err >&2; touch generated.txt')
python3 - "$run_json" <<'PY'
import json, sys
record = json.loads(sys.argv[1])
assert record["status"] == "ok", record
assert record["registry_changed"] is False, record
assert record["cache_changed"] is False, record
assert record["git_changed"] is False, record
assert record["view"]["type"] == "run", record
view = record["view"]["data"]
assert view["state_ref"].startswith("patch:"), view
assert view["resolved_state"].startswith("graft-tree:tree:"), view
assert view["cwd"] == "worktrees/A", view
assert view["exit_code"] == 0, view
assert view["stdout"] == "run-ok", view
assert view["stderr"] == "run-err", view
PY

root_run_json=$("$GRAFT" --json run "$patch" -- /bin/sh -c 'test -d worktrees/A; pwd >/dev/null')
python3 - "$root_run_json" <<'PY'
import json, sys
view = json.loads(sys.argv[1])["view"]["data"]
assert view["cwd"] == ".", view
assert view["exit_code"] == 0, view
PY

materialize_out=$("$GRAFT" patch materialize "$patch")
materialized_path=$(extract_materialize_path <<<"$materialize_out")
[[ -n $materialized_path ]] || { echo "FAIL: materialize did not report output path"; echo "$materialize_out"; exit 1; }
[[ -e "$materialized_path/worktrees/A/README.md" ]] || { echo "FAIL: materialized state missing repo path"; exit 1; }
[[ ! -e "$materialized_path/worktrees/A/generated.txt" ]] || { echo "FAIL: graft run write leaked into materialized state"; exit 1; }

if [[ -d .graft/store/derived/evidence ]] && find .graft/store/derived/evidence -type f | grep -q .; then
  echo "FAIL: graft run unexpectedly wrote evidence"
  find .graft/store/derived/evidence -type f
  exit 1
fi

echo "OK: graft run executes inside temporary whole-state roots and discards writes."
