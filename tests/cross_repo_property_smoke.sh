#!/usr/bin/env bash
# tests/cross_repo_property_smoke.sh
#
# Verifies properties are whole-state checks: cross-repo logic reads
# worktrees/<repo-id> inside app.target(), and repo-prefixed property refs are
# rejected.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
trap cleanup_workspace EXIT

cd "$WORKDIR"
"$GRAFT" init >/dev/null

cat > properties.roto <<'ROTO'
fn repo_outputs_match(app: Application) -> Property {
    let run = call(
        ["/bin/sh", "-c", "cmp worktrees/A/value.txt worktrees/B/value.txt"],
        app.target(),
    );
    property(
        [run.exit_code_is(0).success()],
        "repo A and B expose the same value in the whole target state",
        Severity.Blocking,
        [],
    )
}
ROTO
"$GRAFT" property lock >/dev/null

scratch_a_out=$("$GRAFT" scratch write --base graft:empty worktrees/A/value.txt --content $'same\n')
scratch_a=$(grep -oE 'scratch:[0-9a-f]+' <<<"$scratch_a_out" | tail -n1)
[[ -n $scratch_a ]] || { echo "FAIL: scratch A write did not return scratch id"; echo "$scratch_a_out"; exit 1; }

scratch_b_out=$("$GRAFT" scratch write --from "$scratch_a" worktrees/B/value.txt --content $'same\n')
scratch_b=$(grep -oE 'scratch:[0-9a-f]+' <<<"$scratch_b_out" | tail -n1)
[[ -n $scratch_b ]] || { echo "FAIL: scratch B write did not return scratch id"; echo "$scratch_b_out"; exit 1; }

candidate_out=$("$GRAFT" patch from-scratch "$scratch_b" --expect repo_outputs_match --message cross-repo-property)
candidate=$(first_graft_id candidate "$candidate_out")
[[ -n $candidate ]] || { echo "FAIL: patch from-scratch did not return candidate id"; echo "$candidate_out"; exit 1; }

validate_out=$("$GRAFT" patch validate "$candidate")
grep -q 'validation completed' <<<"$validate_out" || { echo "FAIL: cross-repo property validate did not complete"; echo "$validate_out"; exit 1; }
grep -q 'repo_outputs_match' <<<"$validate_out" || { echo "FAIL: cross-repo property evidence missing"; echo "$validate_out"; exit 1; }
grep -q 'passed' <<<"$validate_out" || { echo "FAIL: cross-repo property did not pass"; echo "$validate_out"; exit 1; }

admit_out=$("$GRAFT" patch admit "$candidate" --require repo_outputs_match)
patch=$(first_graft_id patch "$admit_out")
[[ -n $patch ]] || { echo "FAIL: admit did not return patch id"; echo "$admit_out"; exit 1; }

if "$GRAFT" patch validate "$patch" --expect A:repo_outputs_match >/tmp/graft-repo-scope-property.out 2>&1; then
  echo "FAIL: repo-prefixed property ref unexpectedly validated"
  cat /tmp/graft-repo-scope-property.out
  exit 1
fi
grep -Fq '[E_SCOPED_PROPERTY_UNSUPPORTED]' /tmp/graft-repo-scope-property.out || {
  echo "FAIL: repo-prefixed property rejection did not explain bare-name whole-state scope"
  cat /tmp/graft-repo-scope-property.out
  exit 1
}

echo "OK: whole-state property can compare worktrees/A and worktrees/B without repo-prefixed property refs."
