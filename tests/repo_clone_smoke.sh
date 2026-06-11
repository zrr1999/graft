#!/usr/bin/env bash
# tests/repo_clone_smoke.sh
#
# Verifies project-level [repos] config, graft repo add/sync/lock/update,
# plus the current scratch -> candidate lifecycle used for candidate creation.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
trap cleanup_workspace EXIT
SOURCE="$WORKDIR/source"
PROJECT="$WORKDIR/project"
mkdir -p "$SOURCE" "$PROJECT"

git -C "$SOURCE" init -b main >/dev/null
git -C "$SOURCE" config user.email smoke@example.invalid
git -C "$SOURCE" config user.name "Graft Smoke"
git -C "$SOURCE" config commit.gpgsign false
printf 'demo\n' > "$SOURCE/README.md"
git -C "$SOURCE" add README.md
git -C "$SOURCE" commit -m base >/dev/null
base_tree=$(git -C "$SOURCE" rev-parse 'HEAD^{tree}')

cd "$PROJECT"
"$GRAFT_BIN" init >/dev/null
repo_add_help=$("$GRAFT_BIN" repo add --help)
if grep -q -- '--path' <<<"$repo_add_help"; then
  echo "FAIL: repo add still exposes --path"
  echo "$repo_add_help"; exit 1
fi

add=$("$GRAFT_BIN" repo add demo "$SOURCE")
if ! grep -q "$base_tree" <<<"$add"; then
  echo "FAIL: repo add did not auto-lock the source base tree"
  echo "$add"; exit 1
fi
if ! grep -q 'default_branch = "main"' graft.toml; then
  echo "FAIL: repo add did not record the remote default branch"
  cat graft.toml; exit 1
fi
if ! grep -Fq "url = \"$SOURCE\"" graft.lock; then
  echo "FAIL: repo add did not record the repo url in graft.lock"
  cat graft.lock; exit 1
fi
if [[ -e demo ]]; then
  echo "FAIL: repo add created a top-level demo checkout"
  ls -la; exit 1
fi

list_after_add=$("$GRAFT_BIN" repo list)
if ! grep -q $'demo\tpresent' <<<"$list_after_add"; then
  echo "FAIL: repo list did not report present demo repo after auto-lock"
  echo "$list_after_add"; exit 1
fi
if ! grep -q '.graft/repos/demo' <<<"$list_after_add"; then
  echo "FAIL: repo list did not use the default Graft repo cache"
  echo "$list_after_add"; exit 1
fi

sync=$("$GRAFT_BIN" repo sync demo)
if ! grep -q $'demo\tsynced' <<<"$sync"; then
  echo "FAIL: repo sync did not fetch the existing managed repo cache"
  echo "$sync"; exit 1
fi

lock=$("$GRAFT_BIN" repo lock demo)
if ! grep -q "$base_tree" <<<"$lock"; then
  echo "FAIL: explicit repo lock did not preserve the source base tree"
  echo "$lock"; exit 1
fi

repo_read=$("$GRAFT_BIN" scratch read --repo demo --base main README.md --mode text)
if ! grep -q 'demo' <<<"$repo_read"; then
  echo "FAIL: scratch read --repo demo --base main did not read the locked repo tree"
  echo "$repo_read"; exit 1
fi

repo_write=$("$GRAFT_BIN" scratch write --repo demo --base main README.md --content $'demo changed\n')
repo_scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$repo_write" | tail -n1)
[[ -n $repo_scratch ]] || { echo "FAIL: scratch write --repo demo did not return scratch id"; echo "$repo_write"; exit 1; }
repo_candidate_out=$("$GRAFT_BIN" patch from-scratch "$repo_scratch" --message repo-base-context)
repo_candidate=$(first_graft_id candidate "$repo_candidate_out")
[[ -n $repo_candidate ]] || { echo "FAIL: scratch --repo base-context did not become a candidate"; echo "$repo_candidate_out"; exit 1; }
repo_validate=$("$GRAFT_BIN" patch validate "$repo_candidate")
grep -q 'validation completed' <<<"$repo_validate" || { echo "FAIL: scratch --repo base-context candidate did not validate"; echo "$repo_validate"; exit 1; }

mkdir -p src
printf 'changed\n' > src/new.txt
scratch_out=$("$GRAFT_BIN" scratch write --base graft:empty src/new.txt --content $'changed\n')
scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$scratch_out" | tail -n1)
[[ -n $scratch ]] || { echo "FAIL: no scratch id captured"; echo "$scratch_out"; exit 1; }
create=$("$GRAFT_BIN" patch from-scratch "$scratch" --message repo-smoke-candidate)
candidate=$(first_graft_id candidate "$create")
[[ -n $candidate ]] || { echo "FAIL: no candidate id captured"; echo "$create"; exit 1; }

validate=$("$GRAFT_BIN" patch validate "$candidate" --json)
python3 - "$validate" <<'PY'
import json, sys
record = json.loads(sys.argv[1])
assert record["evidence"] == [], record
PY

admit=$("$GRAFT_BIN" patch admit "$candidate")
patch=$(first_graft_id patch "$admit")
[[ -n $patch ]] || { echo "FAIL: scratch-based candidate did not admit"; echo "$admit"; exit 1; }

printf 'updated\n' > "$SOURCE/README.md"
git -C "$SOURCE" add README.md
git -C "$SOURCE" commit -m update >/dev/null
new_tree=$(git -C "$SOURCE" rev-parse 'HEAD^{tree}')

update=$("$GRAFT_BIN" repo update demo)
if ! grep -q "$new_tree" <<<"$update"; then
  echo "FAIL: repo update did not record the moved source tree"
  echo "$update"; exit 1
fi
if grep -q "$base_tree" <<<"$update"; then
  echo "FAIL: repo update still reported the old tree"
  echo "$update"; exit 1
fi

echo "OK: repo add auto-locks, sync, lock/update, scratch --repo base-context resolution, candidate integrity, and admit work."
