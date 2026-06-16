#!/usr/bin/env bash
# tests/candidate_from_scratch_smoke.sh
#
# Minimal lifecycle smoke for the canonical scratch -> candidate entrypoint:
# scratch write --base -> scratch edit/delete --from -> patch from-scratch
# -> patch validate -> patch admit -> patch materialize dry-run.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
PROJECT="$WORKDIR/project"
mkdir -p "$PROJECT"
trap cleanup_workspace EXIT
require_local_socket_bind

cd "$PROJECT"
"$GRAFT" init >/dev/null

seed_note_out=$("$GRAFT" scratch write --base graft:empty note.txt --content $'alpha\nbeta\n')
scratch_note=$(grep -oE 'scratch:[0-9a-f]+' <<<"$seed_note_out" | tail -n1)
[[ -n $scratch_note ]] || { echo "FAIL: seed note write did not return scratch id"; echo "$seed_note_out"; exit 1; }

seed_remove_out=$("$GRAFT" scratch write --from "$scratch_note" remove.txt --content $'remove me\n')
scratch_seed=$(grep -oE 'scratch:[0-9a-f]+' <<<"$seed_remove_out" | tail -n1)
[[ -n $scratch_seed ]] || { echo "FAIL: seed remove write did not return scratch id"; echo "$seed_remove_out"; exit 1; }

seed_candidate_out=$("$GRAFT" patch from-scratch "$scratch_seed")
seed_candidate=$(first_graft_id candidate "$seed_candidate_out")
[[ -n $seed_candidate ]] || { echo "FAIL: seed candidate from-scratch without message did not return candidate id"; echo "$seed_candidate_out"; exit 1; }

write_out=$("$GRAFT" scratch write --base "$seed_candidate" added.txt --content $'added\n')
scratch_write=$(grep -oE 'scratch:[0-9a-f]+' <<<"$write_out" | tail -n1)
[[ -n $scratch_write ]] || { echo "FAIL: scratch write did not return scratch id"; echo "$write_out"; exit 1; }
grep -q 'added.txt' <<<"$write_out" || { echo "FAIL: write changed_paths missing added.txt"; echo "$write_out"; exit 1; }

read_out=$("$GRAFT" scratch read --from "$scratch_write" note.txt --mode hashlines)
anchor_line=$(grep -oE '^2#[^:]+:beta' <<<"$read_out" || true)
[[ -n $anchor_line ]] || { echo "FAIL: could not read fresh note.txt anchor"; echo "$read_out"; exit 1; }
anchor_hash=${anchor_line#2#}
anchor_hash=${anchor_hash%%:*}
edits_json=$(printf '[{"kind":"replace_line","line":2,"hash":"%s","old":"beta","new":"gamma"}]' "$anchor_hash")
edit_out=$("$GRAFT" scratch edit --from "$scratch_write" note.txt --edits "$edits_json")
scratch_edit=$(grep -oE 'scratch:[0-9a-f]+' <<<"$edit_out" | tail -n1)
[[ -n $scratch_edit ]] || { echo "FAIL: scratch edit did not return scratch id"; echo "$edit_out"; exit 1; }
grep -q 'note.txt' <<<"$edit_out" || { echo "FAIL: edit changed_paths missing note.txt"; echo "$edit_out"; exit 1; }

delete_out=$("$GRAFT" scratch delete --from "$scratch_edit" remove.txt)
scratch_delete=$(grep -oE 'scratch:[0-9a-f]+' <<<"$delete_out" | tail -n1)
[[ -n $scratch_delete ]] || { echo "FAIL: scratch delete did not return scratch id"; echo "$delete_out"; exit 1; }
grep -q 'remove.txt' <<<"$delete_out" || { echo "FAIL: delete changed_paths missing remove.txt"; echo "$delete_out"; exit 1; }

candidate_out=$("$GRAFT" patch from-scratch "$scratch_delete" --message scratch-candidate)
candidate=$(first_graft_id candidate "$candidate_out")
[[ -n $candidate ]] || { echo "FAIL: candidate from-scratch did not return candidate id"; echo "$candidate_out"; exit 1; }
for path in added.txt note.txt remove.txt; do
  grep -q "$path" <<<"$candidate_out" || { echo "FAIL: candidate changed_paths missing $path"; echo "$candidate_out"; exit 1; }
done

candidate_materialize_out=$("$GRAFT" patch materialize "$candidate" --dry-run)
candidate_materialized_path=$(extract_materialize_path <<<"$candidate_materialize_out")
[[ -n $candidate_materialized_path ]] || { echo "FAIL: candidate materialize dry-run did not report output path"; echo "$candidate_materialize_out"; exit 1; }
[[ "$candidate_materialized_path" != *"$candidate"* ]] || { echo "FAIL: candidate materialize output path used candidate id"; echo "$candidate_materialized_path"; exit 1; }

validate_out=$("$GRAFT" patch validate "$candidate")
grep -q 'validation completed' <<<"$validate_out" || { echo "FAIL: validate did not complete"; echo "$validate_out"; exit 1; }

admit_out=$("$GRAFT" patch admit "$candidate")
patch=$(first_graft_id patch "$admit_out")
[[ -n $patch ]] || { echo "FAIL: admit did not return patch id"; echo "$admit_out"; exit 1; }

materialize_out=$("$GRAFT" patch materialize "$patch" --dry-run)
grep -q 'materialization dry-run' <<<"$materialize_out" || { echo "FAIL: materialize dry-run did not report plan"; echo "$materialize_out"; exit 1; }
materialized_path=$(extract_materialize_path <<<"$materialize_out")
[[ -n $materialized_path ]] || { echo "FAIL: materialize dry-run did not report output path"; echo "$materialize_out"; exit 1; }
[[ "$materialized_path" == "$candidate_materialized_path" ]] || { echo "FAIL: candidate and patch materialize resolved to different state paths"; echo "candidate: $candidate_materialized_path"; echo "patch: $materialized_path"; exit 1; }
[[ "$materialized_path" != *"$patch"* ]] || { echo "FAIL: materialize dry-run output path used patch id"; echo "$materialized_path"; exit 1; }
[[ ! -e "$materialized_path" ]] || { echo "FAIL: materialize dry-run unexpectedly wrote worktree"; exit 1; }

echo "OK: patch from-scratch write/edit/delete -> validate -> admit -> materialize dry-run lifecycle works."
