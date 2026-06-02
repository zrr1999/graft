#!/usr/bin/env bash
# tests/evidence_verifier_change_smoke.sh
#
# Verifies evidence is bound to the current verifier definition. A passing
# proof produced under an older command must not satisfy admission after
# graft-properties.toml changes the verifier for the same property.

set -euo pipefail

cd "$(dirname "$0")/.."

cargo build -p graft-cli -p graft-daemon >/dev/null 2>&1
GRAFT_BIN="$PWD/target/debug/graft"
GRAFTD_BIN="$PWD/target/debug/graftd"

WORKDIR="$(mktemp -d)"
cleanup() {
  find "$WORKDIR" -path '*/.graft/run/daemon.sock' -type s -exec "$GRAFTD_BIN" stop --socket {} \; >/dev/null 2>&1 || true
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

cd "$WORKDIR"
printf 'hello\n' > hello.txt
"$GRAFT_BIN" init >/dev/null
cat >> graft-properties.toml <<'EOF'

[[properties]]
name = "Policy"

[properties.query]
kind = "target_snapshot"

[properties.evaluator]
kind = "command"
command = "true"
args = []
env = {}
setup = []
pre = []
teardown = []

[properties.judge]
kind = "exit_code_zero"
EOF
"$GRAFT_BIN" property lock >/dev/null

created=$("$GRAFT_BIN" create --from graft:empty --expect Policy --message verifier-change)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$created" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no candidate captured"; echo "$created"; exit 1; }

"$GRAFT_BIN" validate "$candidate" --expect ValidPatch >/dev/null
old_pass=$("$GRAFT_BIN" validate "$candidate" --expect Policy)
if ! grep -q 'passed' <<<"$old_pass"; then
  echo "FAIL: initial Policy verifier should pass"
  echo "$old_pass"; exit 1
fi

python3 - <<'PY'
from pathlib import Path

path = Path("graft-properties.toml")
text = path.read_text()
path.write_text(text.replace('command = "true"', 'command = "false"', 1))
PY
"$GRAFT_BIN" property lock >/dev/null

if "$GRAFT_BIN" admit "$candidate" --require Policy >admit-ok.out 2>admit-err.out; then
  echo "FAIL: admit reused evidence from an older verifier definition"
  cat admit-ok.out
  exit 1
fi
if ! grep -q '\[A001\] missing required evidence for `Policy`' admit-err.out; then
  echo "FAIL: admit should require evidence for the current verifier definition"
  cat admit-err.out; exit 1
fi

current_failed=$("$GRAFT_BIN" validate "$candidate" --expect Policy)
if ! grep -q 'failed:' <<<"$current_failed"; then
  echo "FAIL: current Policy verifier should fail"
  echo "$current_failed"; exit 1
fi
if "$GRAFT_BIN" admit "$candidate" --require Policy >admit-ok2.out 2>admit-err2.out; then
  echo "FAIL: admit accepted current failed verifier evidence"
  cat admit-ok2.out
  exit 1
fi
if ! grep -q '\[A002\] evidence for `Policy` did not pass' admit-err2.out; then
  echo "FAIL: admit should report failed current evidence after revalidation"
  cat admit-err2.out; exit 1
fi

echo "OK: admission does not reuse stale verifier evidence after graft-properties.toml changes."
