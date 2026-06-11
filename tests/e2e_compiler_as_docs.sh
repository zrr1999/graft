#!/usr/bin/env bash
# tests/e2e_compiler_as_docs.sh
#
# End-to-end smoke for the compiler-as-documentation thread: help/about text,
# workflow guidance, diagnostic codes, property warnings, explain lookup, and
# Hole Report all come from the implemented structural layers.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins

help=$("$GRAFT_BIN" --help)
if ! grep -q 'Draft, validate and admit patch candidates' <<<"$help"; then
  echo "FAIL: top-level help missing long_about text"
  echo "$help"; exit 1
fi
if ! grep -q 'explain' <<<"$help"; then
  echo "FAIL: top-level help missing explain command"
  echo "$help"; exit 1
fi
if grep -qE '^[[:space:]]+learn[[:space:]]' <<<"$help"; then
  echo "FAIL: top-level help still exposes retired guided walkthrough command"
  echo "$help"; exit 1
fi
workflow=$("$GRAFT_BIN" explain agent-workflow)
if ! grep -q 'Recommended workflow for agents and pi-graft tools' <<<"$workflow"; then
  echo "FAIL: agent workflow help missing recommended workflow guidance"
  echo "$workflow"; exit 1
fi

NOGIT="$(mktemp -d)"
GRAFT_HOME="$NOGIT/graft-home"
export GRAFT_HOME
cleanup() {
  cleanup_daemon "$GRAFT_HOME/run/daemon.sock"
  rm -rf "$NOGIT"
}
trap cleanup EXIT

cd "$NOGIT"
printf 'nogit\n' > hello.txt
"$GRAFT_BIN" workspace init >/dev/null

# 1) `scratch write --base graft:empty` is the documented no-git starting
#    point; `candidate from-scratch` then creates a real candidate.
scratch_out=$("$GRAFT_BIN" scratch write --base graft:empty hello.txt --content $'nogit\n')
scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$scratch_out" | tail -n1)
[[ -n $scratch ]] || { echo "FAIL: no scratch from --base graft:empty"; echo "$scratch_out"; exit 1; }
create=$("$GRAFT_BIN" patch from-scratch "$scratch" --message nogit-empty)
candidate=$(first_graft_id candidate "$create")
[[ -n $candidate ]] || { echo "FAIL: no candidate from candidate from-scratch"; echo "$create"; exit 1; }

# 2) Validation must pass and render the Hole Report next-action block.
validate=$("$GRAFT_BIN" patch validate "$candidate")
if ! grep -qE '^next:$' <<<"$validate" || ! grep -q '\[recommended\]' <<<"$validate"; then
  echo "FAIL: validate output missing Hole Report"
  echo "$validate"; exit 1
fi
search=$("$GRAFT_BIN" patch search --property TestsPass 2>&1 >/dev/null || true)
if ! grep -q 'property `TestsPass` is not declared' <<<"$search"; then
  echo "FAIL: search unknown property warning missing"
  echo "$search"; exit 1
fi

for spec in 'admit:^concept: admit$' 'V003:^diagnostic: V003$' 'changed_paths_any_match:^builtin evaluator: changed_paths_any_match$'; do
  id=${spec%%:*}
  pattern=${spec#*:}
  out=$("$GRAFT_BIN" explain "$id")
  if ! grep -qE "$pattern" <<<"$out"; then
    echo "FAIL: explain $id did not match $pattern"
    echo "$out"; exit 1
  fi
done

echo "OK: compiler-as-docs e2e covers help, workflow guidance, V003, Hole Report, warnings, and explain."
