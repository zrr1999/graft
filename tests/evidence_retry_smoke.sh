#!/usr/bin/env bash
# tests/evidence_retry_smoke.sh
#
# Verifies admission policy reads the evidence set semantically: a later
# passing proof for the same property satisfies the gate even when an earlier
# failed attempt is retained for audit history.

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
name = "RetryPasses"

[properties.query]
kind = "target_snapshot"

[properties.evaluator]
kind = "command"
command = "false"
args = []
env = {}
setup = []
pre = []
teardown = []

[properties.judge]
kind = "exit_code_zero"
EOF
"$GRAFT_BIN" property lock >/dev/null

created=$("$GRAFT_BIN" create --from graft:empty --expect ValidPatch --message retry-evidence)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$created" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no candidate captured"; echo "$created"; exit 1; }

"$GRAFT_BIN" validate "$candidate" --expect ValidPatch >/dev/null

failed=$("$GRAFT_BIN" validate "$candidate" --expect RetryPasses)
if ! grep -q 'failed:' <<<"$failed"; then
  echo "FAIL: first validation should record failed evidence"
  echo "$failed"; exit 1
fi

python3 - <<'PY'
from pathlib import Path

path = Path("graft-properties.toml")
text = path.read_text()
path.write_text(text.replace('command = "false"', 'command = "true"', 1))
PY
"$GRAFT_BIN" property lock >/dev/null

passed=$("$GRAFT_BIN" validate "$candidate" --expect RetryPasses)
if ! grep -q 'passed' <<<"$passed"; then
  echo "FAIL: second validation should record passing evidence"
  echo "$passed"; exit 1
fi

admitted=$("$GRAFT_BIN" admit "$candidate" --require ValidPatch)
if ! grep -qE 'admitted patch patch:[0-9a-f]+ from candidate' <<<"$admitted"; then
  echo "FAIL: admit should accept independent passing ValidPatch evidence even after RetryPasses is retried"
  echo "$admitted"; exit 1
fi

echo "OK: admission ignores unrelated failed evidence and accepts a passing required property."
