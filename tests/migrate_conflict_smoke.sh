#!/usr/bin/env bash
# tests/migrate_conflict_smoke.sh
#
# Verifies that migration preserves patch intent: a patch that modified an
# existing file must not silently become an added-file patch when the new base
# lacks that path.

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
git init >/dev/null
git config user.email smoke@example.invalid
git config user.name "Graft Smoke"
git config commit.gpgsign false
mkdir -p src
printf 'old\n' > src/lib.rs
git add src/lib.rs
git commit -m base >/dev/null

"$GRAFT_BIN" init >/dev/null
printf 'new\n' > src/lib.rs
created=$("$GRAFT_BIN" create --expect ValidPatch --message migrate-modified-file)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$created" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no candidate captured"; echo "$created"; exit 1; }
"$GRAFT_BIN" validate "$candidate" >/dev/null
admitted=$("$GRAFT_BIN" admit "$candidate" --require ValidPatch)
patch=$(grep -oE 'patch:[0-9a-f]+' <<<"$admitted" | head -n1)
[[ -n $patch ]] || { echo "FAIL: no patch captured"; echo "$admitted"; exit 1; }

if "$GRAFT_BIN" migrate "$patch" --onto graft:empty >migrate-ok.out 2>migrate-err.out; then
  echo "FAIL: migration should fail loud with E_COMPOSE_CONFLICT"
  cat migrate-ok.out
  exit 1
fi
migrated=$(cat migrate-err.out)
if ! grep -q "\[E_COMPOSE_CONFLICT\]" <<<"$migrated"; then
  echo "FAIL: migrate did not report E_COMPOSE_CONFLICT"
  echo "$migrated"; exit 1
fi
if ! grep -q "modified path is missing on new base" <<<"$migrated"; then
  echo "FAIL: migrate did not explain the missing modified base path"
  echo "$migrated"; exit 1
fi

echo "OK: migrate refuses to turn a modified-file patch into an added-file patch."
