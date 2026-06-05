#!/usr/bin/env bash
# tests/evidence_retry_smoke.sh
#
# Verifies admission policy reads the evidence set semantically: a later
# passing proof for the same property satisfies the gate even when an earlier
# failed attempt is retained for audit history.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
trap cleanup_workspace EXIT

cd "$WORKDIR"
printf 'hello\n' > hello.txt
"$GRAFT_BIN" init >/dev/null
write_properties_roto <<'ROTO'
fn retry_passes(app: Application) -> Property {
    let run = call(["false"], app.target());

    property(
        [
            run.exit_code_is(0).success(),
        ],
        "retry verifier that can be changed from failing to passing",
        Severity.Blocking,
        [],
    )
}
ROTO
lock_properties

scratch_out=$("$GRAFT_BIN" scratch write --base graft:empty hello.txt --content $'hello\n')
scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$scratch_out" | tail -n1)
[[ -n $scratch ]] || { echo "FAIL: no scratch captured"; echo "$scratch_out"; exit 1; }
created=$("$GRAFT_BIN" candidate from-scratch "$scratch" --message retry-evidence)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$created" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no candidate captured"; echo "$created"; exit 1; }

failed=$("$GRAFT_BIN" validate "$candidate" --expect workspace:retry_passes)
if ! grep -q 'failed:' <<<"$failed"; then
  echo "FAIL: first validation should record failed evidence"
  echo "$failed"; exit 1
fi

python3 - <<'PY'
from pathlib import Path

path = Path("properties.roto")
text = path.read_text()
path.write_text(text.replace('["false"]', '["true"]', 1))
PY
lock_properties

passed=$("$GRAFT_BIN" validate "$candidate" --expect workspace:retry_passes)
if ! grep -q 'passed' <<<"$passed"; then
  echo "FAIL: second validation should record passing evidence"
  echo "$passed"; exit 1
fi

admitted=$("$GRAFT_BIN" admit "$candidate" --require workspace:retry_passes)
if ! grep -qE 'admitted patch patch:[0-9a-f]+ from candidate' <<<"$admitted"; then
  echo "FAIL: admit should accept latest passing retry_passes evidence after retry"
  echo "$admitted"; exit 1
fi

echo "OK: admission ignores unrelated failed evidence and accepts a passing required property."
