#!/usr/bin/env bash
# tests/diagnostics_smoke.sh
#
# Verifies that user-facing diagnostics surface as `[CODE] ... — fix — see:`
# instead of leaking raw upstream errors. Backs the @diagnostic-enum-and-codes
# task evidence requirement.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
trap cleanup_workspace EXIT

cd "$WORKDIR"
echo "smoke" > hello.txt

"$GRAFT_BIN" init >/dev/null

# 1) `scratch write --base graft:empty` + `candidate from-scratch` produces a
#    real candidate whose core patch integrity passes, even with no git context.
scratch_out=$("$GRAFT_BIN" scratch write --base graft:empty hello.txt --content $'smoke\n')
scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$scratch_out" | tail -n1)
[[ -n $scratch ]] || { echo "FAIL: no scratch id captured for graft:empty base"; echo "$scratch_out"; exit 1; }
out=$("$GRAFT_BIN" candidate from-scratch "$scratch" --message empty-base-smoke)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$out" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no candidate id captured for graft:empty base"; echo "$out"; exit 1; }
validate_out=$("$GRAFT_BIN" validate "$candidate")
if ! grep -q 'validation completed' <<<"$validate_out"; then
  echo "FAIL: core validation did not complete against graft:empty base"
  echo "$validate_out"
  exit 1
fi

write_properties_roto <<'ROTO'
fn touches_unvalidated(app: Application) -> Property {
    property(
        [
            app.changed_paths().any_match(["unvalidated.txt"]).success(),
        ],
        "change touches the intentionally unvalidated smoke file",
        Severity.Blocking,
        [],
    )
}
ROTO
lock_properties

# 2) Admit without passing evidence for an explicit property must surface A001/A002 diagnostics.
scratch_out2=$("$GRAFT_BIN" scratch write --base graft:empty unvalidated.txt --content $'unvalidated\n')
scratch2=$(grep -oE 'scratch:[0-9a-f]+' <<<"$scratch_out2" | tail -n1)
[[ -n $scratch2 ]] || { echo "FAIL: no scratch captured for admit smoke"; echo "$scratch_out2"; exit 1; }
out2=$("$GRAFT_BIN" candidate from-scratch "$scratch2" --expect workspace:touches_unvalidated --message admit-smoke)
candidate2=$(grep -oE 'candidate:[0-9a-f]+' <<<"$out2" | head -n1)
[[ -n $candidate2 ]] || { echo "FAIL: no candidate captured for admit smoke"; echo "$out2"; exit 1; }
if "$GRAFT_BIN" admit "$candidate2" --require workspace:touches_unvalidated >/tmp/admit-ok 2>/tmp/admit-err; then
  echo "FAIL: admit unexpectedly succeeded"
  cat /tmp/admit-ok
  exit 1
fi
if ! grep -qE '\[A00[12]\]' /tmp/admit-err; then
  echo "FAIL: admit failure did not surface an [A00x] diagnostic"
  cat /tmp/admit-err
  exit 1
fi

echo "OK: scratch/candidate no-git path works and A00x diagnostics surface with code, fix, and see-also."
