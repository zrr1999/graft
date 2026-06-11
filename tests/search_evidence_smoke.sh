#!/usr/bin/env bash
# tests/search_evidence_smoke.sh
#
# `graft search --has-evidence <property>` promises passing evidence. Failed
# evidence must remain visible on the patch record without being reported as a
# proof that the patch has the property.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
trap cleanup_workspace EXIT

cd "$WORKDIR"
"$GRAFT_BIN" init >/dev/null
write_properties_roto <<'ROTO'
fn touches_file(app: Application) -> Property {
    property(
        [
            app.changed_paths().any_match(["file.txt"]).success(),
        ],
        "change touches file.txt",
        Severity.Blocking,
        [],
    )
}

fn never_pass(app: Application) -> Property {
    let run = call(["false"], app.target());

    property(
        [
            run.exit_code_is(0).success(),
        ],
        "command verifier that intentionally fails",
        Severity.Blocking,
        [],
    )
}
ROTO
lock_properties

printf 'x\n' > file.txt
scratch_out=$("$GRAFT_BIN" scratch write --base graft:empty file.txt --content $'x\n')
scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$scratch_out" | tail -n1)
[[ -n $scratch ]] || { echo "FAIL: no scratch id"; echo "$scratch_out"; exit 1; }
create=$("$GRAFT_BIN" patch from-scratch "$scratch" --expect touches_file --message search-evidence)
candidate=$(first_graft_id candidate "$create")
[[ -n $candidate ]] || { echo "FAIL: no candidate id"; echo "$create"; exit 1; }

"$GRAFT_BIN" patch validate "$candidate" --expect touches_file >/dev/null
"$GRAFT_BIN" patch validate "$candidate" --expect never_pass >/dev/null
admit=$("$GRAFT_BIN" patch admit "$candidate" --require touches_file)
patch=$(first_graft_id patch "$admit")
[[ -n $patch ]] || { echo "FAIL: no patch id"; echo "$admit"; exit 1; }

valid_search=$("$GRAFT_BIN" patch search --has-evidence touches_file --json)
failed_search=$("$GRAFT_BIN" patch search --has-evidence never_pass --json)

python3 - "$valid_search" "$failed_search" "$patch" <<'PY'
import json, sys
valid_search, failed_search, patch = sys.argv[1:]
valid = json.loads(valid_search)["patch_ids"]
failed = json.loads(failed_search)["patch_ids"]
assert patch in valid, (patch, valid)
assert patch not in failed, (patch, failed)
PY

echo "OK: search --has-evidence only reports passing evidence."
