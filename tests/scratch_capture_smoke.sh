#!/usr/bin/env bash
# tests/scratch_capture_smoke.sh
#
# Verifies stash-like scratch capture: cwd changes become a scratch, captured
# paths are restored to the base, ignored/generated paths stay in cwd, and
# dry-run does not create scratch state.

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

printf 'first\n' > first.txt
mkdir -p worktrees/A target dist
printf 'ignored repo state\n' > worktrees/A/value.txt
printf 'ignored target\n' > target/artifact
printf 'ignored dist\n' > dist/bundle.js

capture_out=$("$GRAFT" scratch capture --base graft:empty)
scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$capture_out" | head -n1)
[[ -n $scratch ]] || { echo "FAIL: capture did not return scratch id"; echo "$capture_out"; exit 1; }
grep -q 'first.txt' <<<"$capture_out" || { echo "FAIL: capture output missing first.txt"; echo "$capture_out"; exit 1; }
if grep -q 'worktrees/A/value.txt' <<<"$capture_out"; then
  echo "FAIL: capture should ignore top-level worktrees/"
  echo "$capture_out"; exit 1
fi
[[ ! -e first.txt ]] || { echo "FAIL: captured file was not restored away"; exit 1; }
[[ -e worktrees/A/value.txt ]] || { echo "FAIL: ignored worktrees/ file was removed"; exit 1; }
[[ -e target/artifact ]] || { echo "FAIL: ignored target/ file was removed"; exit 1; }
[[ -e dist/bundle.js ]] || { echo "FAIL: ignored dist/ file was removed"; exit 1; }

candidate_out=$("$GRAFT" patch from-scratch "$scratch" --message capture-first)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$candidate_out" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: patch from-scratch did not return candidate"; echo "$candidate_out"; exit 1; }
grep -q 'first.txt' <<<"$candidate_out" || { echo "FAIL: candidate missing captured first.txt"; echo "$candidate_out"; exit 1; }

printf 'second\n' > second.txt
second_out=$("$GRAFT" scratch capture --base graft:empty)
second_scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$second_out" | head -n1)
[[ -n $second_scratch ]] || { echo "FAIL: second capture did not return scratch"; echo "$second_out"; exit 1; }
grep -q 'second.txt' <<<"$second_out" || { echo "FAIL: second capture missing second.txt"; echo "$second_out"; exit 1; }
if grep -q 'first.txt' <<<"$second_out"; then
  echo "FAIL: second capture repeated first capture path"
  echo "$second_out"; exit 1
fi
[[ ! -e second.txt ]] || { echo "FAIL: second captured file was not restored away"; exit 1; }

printf 'dry\n' > dry.txt
dry_out=$("$GRAFT" scratch capture --base graft:empty --dry-run)
dry_scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$dry_out" | head -n1)
[[ -n $dry_scratch ]] || { echo "FAIL: dry-run capture did not report scratch id"; echo "$dry_out"; exit 1; }
[[ -e dry.txt ]] || { echo "FAIL: dry-run mutated cwd"; exit 1; }
if "$GRAFT" patch from-scratch "$dry_scratch" --message dry-run >/tmp/graft-capture-dry.out 2>&1; then
  echo "FAIL: dry-run scratch unexpectedly existed"
  cat /tmp/graft-capture-dry.out; exit 1
fi

rm dry.txt
if "$GRAFT" scratch capture --base graft:empty >/tmp/graft-capture-empty.out 2>&1; then
  echo "FAIL: empty capture unexpectedly succeeded"
  cat /tmp/graft-capture-empty.out; exit 1
fi
grep -q '\[E_EMPTY_CAPTURE\]' /tmp/graft-capture-empty.out || {
  echo "FAIL: empty capture did not report E_EMPTY_CAPTURE"
  cat /tmp/graft-capture-empty.out; exit 1
}

echo "OK: scratch capture stashes cwd changes, preserves ignored dirs, supports dry-run, and rejects empty captures."
