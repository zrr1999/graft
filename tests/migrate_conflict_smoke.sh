#!/usr/bin/env bash
# tests/migrate_conflict_smoke.sh
#
# Verifies that migration preserves patch intent: a patch that modified an
# existing file must not silently become an added-file patch when the new base
# lacks that path.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
trap cleanup_workspace EXIT

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
seed_scratch_out=$("$GRAFT_BIN" scratch write --base graft:empty src/lib.rs --content $'old\n')
seed_scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$seed_scratch_out" | tail -n1)
[[ -n $seed_scratch ]] || { echo "FAIL: no seed scratch captured"; echo "$seed_scratch_out"; exit 1; }
seed_created=$("$GRAFT_BIN" candidate from-scratch "$seed_scratch" --message migrate-base-file)
seed_candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$seed_created" | head -n1)
[[ -n $seed_candidate ]] || { echo "FAIL: no seed candidate captured"; echo "$seed_created"; exit 1; }
"$GRAFT_BIN" validate "$seed_candidate" >/dev/null
seed_admitted=$("$GRAFT_BIN" admit "$seed_candidate")
base_patch=$(grep -oE 'patch:[0-9a-f]+' <<<"$seed_admitted" | head -n1)
[[ -n $base_patch ]] || { echo "FAIL: no base patch captured"; echo "$seed_admitted"; exit 1; }

printf 'new\n' > src/lib.rs
scratch_out=$("$GRAFT_BIN" scratch write --base "$base_patch" src/lib.rs --content $'new\n')
scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$scratch_out" | tail -n1)
[[ -n $scratch ]] || { echo "FAIL: no scratch captured"; echo "$scratch_out"; exit 1; }
created=$("$GRAFT_BIN" candidate from-scratch "$scratch" --message migrate-modified-file)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$created" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no candidate captured"; echo "$created"; exit 1; }
"$GRAFT_BIN" validate "$candidate" >/dev/null
admitted=$("$GRAFT_BIN" admit "$candidate")
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
