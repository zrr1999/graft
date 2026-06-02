#!/usr/bin/env bash
# tests/repo_clone_smoke.sh
#
# Verifies project-level [repos] config, graft repo add/sync, and
# repo:<repo_id>@<treeish> base resolution for candidate creation.

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
"$GRAFT_BIN" repo add demo "$SOURCE" --default-branch main >/dev/null

list_before=$("$GRAFT_BIN" repo list)
if ! grep -q $'demo\tmissing' <<<"$list_before"; then
  echo "FAIL: repo list did not report missing demo repo"
  echo "$list_before"; exit 1
fi

sync=$("$GRAFT_BIN" repo sync demo)
if ! grep -q $'demo\tcloned' <<<"$sync"; then
  echo "FAIL: repo sync did not clone demo repo"
  echo "$sync"; exit 1
fi

mkdir -p src
printf 'changed\n' > src/new.txt
create=$("$GRAFT_BIN" create --from repo:demo@main --expect ValidPatch --message repo-base)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$create" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no candidate id captured"; echo "$create"; exit 1; }
if ! grep -q 'repo:demo@main#' <<<"$create"; then
  echo "FAIL: create output did not show repo-aware base"
  echo "$create"; exit 1
fi
if grep -q '.graft/repos' <<<"$create"; then
  echo "FAIL: .graft repos cache leaked into captured change summary"
  echo "$create"; exit 1
fi

python3 - "$PROJECT" "$candidate" "$base_tree" <<'PY'
import json, pathlib, sys
project, candidate, expected_tree = sys.argv[1:]
root = pathlib.Path(project)
record = json.loads((root / ".graft/store/private/candidate" / f"{candidate}.json").read_text())
base = record["base_state"]
assert base["kind"] == "repo_tree", base
value = base["value"]
assert value["repo_id"] == "demo", value
assert value["treeish"] == "main", value
assert value["resolved_tree_oid"] == expected_tree, value
PY

validate=$("$GRAFT_BIN" validate "$candidate" --expect ValidPatch --json)
python3 - "$validate" <<'PY'
import json, sys
record = json.loads(sys.argv[1])
valid_patch = record["evidence"]
assert valid_patch, record
assert valid_patch[-1]["result"] == "passed", valid_patch
assert valid_patch[-1]["property"].startswith("property:"), valid_patch
PY

admit=$("$GRAFT_BIN" admit "$candidate" --require ValidPatch)
patch=$(grep -oE 'patch:[0-9a-f]+' <<<"$admit" | head -n1)
[[ -n $patch ]] || { echo "FAIL: repo-based ValidPatch evidence did not admit"; echo "$admit"; exit 1; }

printf 'updated\n' > "$SOURCE/README.md"
git -C "$SOURCE" add README.md
git -C "$SOURCE" commit -m update >/dev/null
new_tree=$(git -C "$SOURCE" rev-parse 'HEAD^{tree}')

sync_again=$("$GRAFT_BIN" repo sync demo)
if ! grep -q $'demo\tsynced' <<<"$sync_again"; then
  echo "FAIL: repo sync did not fetch existing demo repo"
  echo "$sync_again"; exit 1
fi

create_after_move=$("$GRAFT_BIN" create --from repo:demo@main --expect ValidPatch --message repo-base-after-move)
candidate_after_move=$(grep -oE 'candidate:[0-9a-f]+' <<<"$create_after_move" | head -n1)
[[ -n $candidate_after_move ]] || { echo "FAIL: no candidate id after branch move"; echo "$create_after_move"; exit 1; }

python3 - "$PROJECT" "$candidate" "$candidate_after_move" "$base_tree" "$new_tree" <<'PY'
import json, pathlib, sys
project, old_candidate, new_candidate, old_tree, expected_new_tree = sys.argv[1:]
root = pathlib.Path(project)
def base_tree(candidate):
    record = json.loads((root / ".graft/store/private/candidate" / f"{candidate}.json").read_text())
    return record["base_state"]["value"]["resolved_tree_oid"]
old_recorded = base_tree(old_candidate)
new_recorded = base_tree(new_candidate)
assert old_recorded == old_tree, (old_recorded, old_tree)
assert new_recorded == expected_new_tree, (new_recorded, expected_new_tree)
assert old_recorded != new_recorded, (old_recorded, new_recorded)
PY

echo "OK: repo add, sync, repo-aware ValidPatch evidence, admit, and branch movement tracking work."
