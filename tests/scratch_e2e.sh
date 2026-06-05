#!/usr/bin/env bash
# tests/scratch_e2e.sh
#
# End-to-end coverage for the scratch protocol seam:
# scratch read/write/edit/delete with --base / --from exercise daemon auto-spawn,
# then candidate from-scratch verifies the scratch -> candidate seam.
#
# This complements tests/scratch_cli_smoke.sh by walking a full
# base→write→edit→delete pipeline against a live graftd.

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
printf 'alpha\nbeta\ngamma\n' > note.txt
"$GRAFT" init >/dev/null

# Anchor base candidate. graft:empty keeps this no-git tempdir self-contained.
seed_write=$("$GRAFT" scratch write --base graft:empty note.txt --content $'alpha\nbeta\ngamma\n')
seed_scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$seed_write" | tail -n1)
[[ -n $seed_scratch ]] || { echo "FAIL: no base scratch captured"; echo "$seed_write"; exit 1; }
create=$("$GRAFT" candidate from-scratch "$seed_scratch" --message scratch-e2e-base)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$create" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no base candidate captured"; echo "$create"; exit 1; }
"$GRAFTD" start --cwd "$PROJECT" --socket "$SOCKET"
[[ -S "$SOCKET" ]] || { echo 'FAIL: explicit global graftd start did not create socket'; exit 1; }

# 1) read directly from the base candidate.
base_read=$("$GRAFT" scratch read --base "$candidate" note.txt --mode text)
grep -q 'alpha' <<<"$base_read" || { echo "FAIL: scratch read --base missing note content"; echo "$base_read"; exit 1; }

# 2) write a brand-new file directly from the base candidate.
write=$("$GRAFT" scratch write --base "$candidate" greeting.txt --content $'hello\nworld\n')
scratch_after_write=$(grep -oE 'scratch:[0-9a-f]+' <<<"$write" | tail -n1)
[[ -n $scratch_after_write ]] || { echo "FAIL: scratch_write did not return new scratch:* id"; echo "$write"; exit 1; }

# 3) edit a file using a real, fresh hashline anchor (read first to discover it).
read_out=$("$GRAFT" scratch read --from "$scratch_after_write" greeting.txt --mode hashlines)
anchor_line=$(grep -oE '^2#[^:]+:world' <<<"$read_out" || true)
[[ -n $anchor_line ]] || { echo "FAIL: could not read fresh anchor for line 2"; echo "$read_out"; exit 1; }
anchor_hash=${anchor_line#2#}
anchor_hash=${anchor_hash%%:*}
edits_json=$(printf '[{"kind":"replace_line","line":2,"hash":"%s","old":"world","new":"graft"}]' "$anchor_hash")
edit=$("$GRAFT" scratch edit --from "$scratch_after_write" greeting.txt --edits "$edits_json")
scratch_after_edit=$(grep -oE 'scratch:[0-9a-f]+' <<<"$edit" | tail -n1)
[[ -n $scratch_after_edit ]] || { echo "FAIL: scratch_edit did not return new scratch:* id"; echo "$edit"; exit 1; }

# 4) delete from the edited scratch and verify diff reports the deleted path.
delete=$("$GRAFT" scratch delete --from "$scratch_after_edit" greeting.txt)
scratch_after_delete=$(grep -oE 'scratch:[0-9a-f]+' <<<"$delete" | tail -n1)
[[ -n $scratch_after_delete ]] || { echo "FAIL: scratch_delete did not return new scratch:* id"; echo "$delete"; exit 1; }

diff=$("$GRAFT" scratch diff "$scratch_after_edit" "$scratch_after_delete")
grep -q 'greeting.txt' <<<"$diff" || { echo "FAIL: scratch diff missing deleted path"; echo "$diff"; exit 1; }

candidate_out=$("$GRAFT" candidate from-scratch "$scratch_after_delete" --message scratch-e2e-final)
final_candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$candidate_out" | head -n1)
[[ -n $final_candidate ]] || { echo "FAIL: no final candidate from scratch"; echo "$candidate_out"; exit 1; }

"$GRAFTD" stop --socket "$SOCKET" >/dev/null

echo "OK: scratch base→write→edit→delete→candidate pipeline runs end-to-end against graftd."
