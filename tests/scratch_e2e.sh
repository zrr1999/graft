#!/usr/bin/env bash
# tests/scratch_e2e.sh
#
# End-to-end coverage for the scratch protocol seam:
# graft create auto-spawns graftd, then scratch open / write / edit / promote
# then validation/admission exercise the same persisted candidate through daemon auto-spawn.
#
# This complements tests/scratch_cli_smoke.sh by walking a full
# open→edit→promote pipeline against a live graftd.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/socket_probe.sh

cargo build -p graft-cli -p graft-daemon >/dev/null 2>&1

GRAFT="$PWD/target/debug/graft"
GRAFTD="$PWD/target/debug/graftd"

WORKDIR="$(mktemp -d)"
PROJECT="$WORKDIR/project"
SOCKET="$PROJECT/.graft/run/daemon.sock"
mkdir -p "$PROJECT"
trap 'if [[ -S "$SOCKET" ]]; then "$GRAFTD" stop --socket "$SOCKET" >/dev/null 2>&1 || true; fi; rm -rf "$WORKDIR"' EXIT
skip_if_local_socket_bind_unavailable "$WORKDIR/probe.sock"

cd "$PROJECT"
printf 'alpha\nbeta\ngamma\n' > note.txt
"$GRAFT" init >/dev/null

# Anchor base candidate. graft:empty keeps this no-git tempdir self-contained.
create=$("$GRAFT" create --from graft:empty --expect ValidPatch --message scratch-e2e-base)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$create" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no base candidate captured"; echo "$create"; exit 1; }

[[ -S "$SOCKET" ]] || { echo 'FAIL: create did not auto-spawn graftd'; exit 1; }

# 1) open scratch from the base candidate.
open=$("$GRAFT" scratch open --base "$candidate")
scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$open" | head -n1)
[[ -n $scratch ]] || { echo "FAIL: scratch_open did not return scratch:* id"; echo "$open"; exit 1; }

# 2) write a brand-new file on top of the scratch.
write=$("$GRAFT" scratch write "$scratch" greeting.txt --content $'hello\nworld\n')
scratch_after_write=$(grep -oE 'scratch:[0-9a-f]+' <<<"$write" | tail -n1)
[[ -n $scratch_after_write ]] || { echo "FAIL: scratch_write did not return new scratch:* id"; echo "$write"; exit 1; }
[[ "$scratch_after_write" != "$scratch" ]] || { echo "FAIL: scratch_write must produce a fresh scratch id"; echo "$write"; exit 1; }

# 3) edit a file using a real, fresh hashline anchor (read first to discover it).
read_out=$("$GRAFT" scratch read "$scratch_after_write" greeting.txt --mode hashlines)
anchor_line=$(grep -oE '^2#[^:]+:world' <<<"$read_out" || true)
[[ -n $anchor_line ]] || { echo "FAIL: could not read fresh anchor for line 2"; echo "$read_out"; exit 1; }
anchor_hash=${anchor_line#2#}
anchor_hash=${anchor_hash%%:*}
edits_json=$(printf '[{"kind":"replace_line","line":2,"hash":"%s","old":"world","new":"graft"}]' "$anchor_hash")
edit=$("$GRAFT" scratch edit "$scratch_after_write" greeting.txt --edits "$edits_json")
scratch_after_edit=$(grep -oE 'scratch:[0-9a-f]+' <<<"$edit" | tail -n1)
[[ -n $scratch_after_edit ]] || { echo "FAIL: scratch_edit did not return new scratch:* id"; echo "$edit"; exit 1; }

# 4) promote the edited scratch into a candidate.
promote=$("$GRAFT" scratch promote "$scratch_after_edit" --expect ValidPatch --producer scratch-e2e --message scratch-e2e-promote)
new_candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$promote" | head -n1)
[[ -n $new_candidate ]] || { echo "FAIL: scratch_promote did not return candidate:* id"; echo "$promote"; exit 1; }
[[ "$new_candidate" != "$candidate" ]] || { echo "FAIL: promote should mint a fresh candidate id"; echo "$promote"; exit 1; }

# 5) the freshly-promoted candidate must validate as a persisted candidate.
"$GRAFTD" stop --socket "$SOCKET" >/dev/null
validated=$("$GRAFT" validate "$new_candidate" --expect ValidPatch)
if ! grep -q 'passed' <<<"$validated"; then
  echo "FAIL: promoted scratch candidate did not validate"
  echo "$validated"; exit 1
fi

echo "OK: scratch open→write→edit→promote→validate pipeline runs end-to-end against graftd."
