#!/usr/bin/env bash
# tests/evidence_verifier_change_smoke.sh
#
# Verifies evidence is bound to the current verifier definition. A passing
# proof produced under an older command must not satisfy admission after
# constraints.roto changes the verifier for the same constraint.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
trap cleanup_workspace EXIT

cd "$WORKDIR"
printf 'hello\n' > hello.txt
"$GRAFT_BIN" init >/dev/null
write_constraints_roto <<'ROTO'
fn policy(app: Application) -> Constraint {
    primitive(app.run(["true"]), exit_zero, "policy command verifier")
}
ROTO
lock_constraints

scratch_out=$("$GRAFT_BIN" scratch write --base graft:empty hello.txt --content $'hello\n')
scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$scratch_out" | tail -n1)
[[ -n $scratch ]] || { echo "FAIL: no scratch captured"; echo "$scratch_out"; exit 1; }
created=$("$GRAFT_BIN" patch from-scratch "$scratch" --expect policy --message verifier-change)
candidate=$(first_graft_id candidate "$created")
[[ -n $candidate ]] || { echo "FAIL: no candidate captured"; echo "$created"; exit 1; }

old_pass=$("$GRAFT_BIN" patch validate "$candidate" --expect policy)
if ! grep -q 'passed' <<<"$old_pass"; then
  echo "FAIL: initial policy verifier should pass"
  echo "$old_pass"; exit 1
fi

python3 - <<'PY'
from pathlib import Path

path = Path("constraints.roto")
text = path.read_text()
path.write_text(text.replace('["true"]', '["false"]', 1))
PY
lock_constraints

if "$GRAFT_BIN" patch admit "$candidate" --require policy >admit-ok.out 2>admit-err.out; then
  echo "FAIL: admit reused evidence from an older verifier definition"
  cat admit-ok.out
  exit 1
fi
if ! grep -q '\[E_CONSTRAINT_DRIFT\]' admit-err.out; then
  echo "FAIL: admit should reject the candidate because its locked policy id drifted"
  cat admit-err.out; exit 1
fi

echo "OK: admission does not reuse stale verifier evidence after constraints.roto changes."
