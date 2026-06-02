#!/usr/bin/env bash
# tests/search_evidence_smoke.sh
#
# `graft search --has-evidence <Property>` promises passing evidence. Failed
# evidence must remain visible on the patch record without being reported as a
# proof that the patch has the property.

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
"$GRAFT_BIN" init >/dev/null
cat >> graft-properties.toml <<'TOML'

[[properties]]
name = "NeverPass"

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
TOML
"$GRAFT_BIN" property lock >/dev/null

printf 'x\n' > file.txt
create=$("$GRAFT_BIN" create --from graft:empty --expect ValidPatch --message search-evidence)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$create" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no candidate id"; echo "$create"; exit 1; }

"$GRAFT_BIN" validate "$candidate" --expect ValidPatch >/dev/null
"$GRAFT_BIN" validate "$candidate" --expect NeverPass >/dev/null
admit=$("$GRAFT_BIN" admit "$candidate" --require ValidPatch)
patch=$(grep -oE 'patch:[0-9a-f]+' <<<"$admit" | head -n1)
[[ -n $patch ]] || { echo "FAIL: no patch id"; echo "$admit"; exit 1; }

valid_search=$("$GRAFT_BIN" search --has-evidence ValidPatch --json)
failed_search=$("$GRAFT_BIN" search --has-evidence NeverPass --json)

python3 - "$valid_search" "$failed_search" "$patch" <<'PY'
import json, sys
valid_search, failed_search, patch = sys.argv[1:]
valid = json.loads(valid_search)["patch_ids"]
failed = json.loads(failed_search)["patch_ids"]
assert patch in valid, (patch, valid)
assert patch not in failed, (patch, failed)
PY

echo "OK: search --has-evidence only reports passing evidence."
