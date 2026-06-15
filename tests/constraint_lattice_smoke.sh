#!/usr/bin/env bash
# tests/constraint_lattice_smoke.sh
#
# Focused end-to-end coverage for constraint-lattice admission/promotion:
# missing then passing one-shot admission requirements, and promotion target
# any_of constraints satisfied by a single passed branch.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
trap cleanup_workspace EXIT

cd "$WORKDIR"
"$GRAFT" init >/dev/null

write_constraints_roto <<'ROTO'
fn b(app: Application) -> Constraint {
    primitive(app.changed_paths(["a.txt"]), any_match, "admission branch b is satisfied by the smoke change")
}

fn x(app: Application) -> Constraint {
    primitive(app.changed_paths(["a.txt"]), any_match, "promotion branch x is satisfied by the smoke change")
}

fn y(app: Application) -> Constraint {
    primitive(app.changed_paths(["never-y.txt"]), any_match, "promotion branch y remains unsatisfied in this smoke")
}
ROTO
lock_constraints

scratch_out=$("$GRAFT" scratch write --base graft:empty a.txt --content $'a\n')
scratch=$(last_graft_id scratch "$scratch_out")
[[ -n $scratch ]] || { echo "FAIL: scratch write did not return scratch id"; echo "$scratch_out"; exit 1; }

candidate_out=$("$GRAFT" patch from-scratch "$scratch" --message constraint-lattice-smoke)
candidate=$(first_graft_id candidate "$candidate_out")
[[ -n $candidate ]] || { echo "FAIL: patch from-scratch did not return candidate id"; echo "$candidate_out"; exit 1; }

if "$GRAFT" patch admit "$candidate" --require b >admit-missing.out 2>admit-missing.err; then
  echo "FAIL: admit --require b succeeded before b evidence existed"
  cat admit-missing.out
  exit 1
fi
if ! grep -q '\[A001\]' admit-missing.err; then
  echo "FAIL: missing admission requirement did not surface A001"
  cat admit-missing.err
  exit 1
fi
if ! grep -q 'b' admit-missing.err; then
  echo "FAIL: missing admission requirement did not name b"
  cat admit-missing.err
  exit 1
fi

validate_b=$("$GRAFT" patch validate "$candidate" --expect b)
grep -q 'passed' <<<"$validate_b" || { echo "FAIL: validating b did not pass"; echo "$validate_b"; exit 1; }
validate_x=$("$GRAFT" patch validate "$candidate" --expect x)
grep -q 'passed' <<<"$validate_x" || { echo "FAIL: validating x did not pass"; echo "$validate_x"; exit 1; }

admit_ok=$("$GRAFT" patch admit "$candidate" --require b)
patch=$(first_graft_id patch "$admit_ok")
[[ -n $patch ]] || { echo "FAIL: admit after b evidence did not return patch id"; echo "$admit_ok"; exit 1; }

TARGET="$WORKDIR/target-git"
mkdir -p "$TARGET"
git -C "$TARGET" init -b main >/dev/null
git -C "$TARGET" config user.email smoke@example.invalid
git -C "$TARGET" config user.name "Graft Smoke"
git -C "$TARGET" config commit.gpgsign false
cat >> graft.toml <<TOML

[promote_targets.out]
path = "$TARGET"
branch = "constraint-out"
required = { any_of = ["x", "y"] }
TOML

promote_out=$("$GRAFT" patch promote "$patch" --to out --yes)
grep -q 'promoted patch' <<<"$promote_out" || { echo "FAIL: promotion did not report success"; echo "$promote_out"; exit 1; }
git -C "$TARGET" rev-parse --verify refs/heads/constraint-out >/dev/null
[[ -n $(find .graft/store/public/promotion -type f -print -quit) ]] || { echo "FAIL: promotion record missing"; exit 1; }

if grep -q 'v2-plan:y@' <<<"$validate_x"; then
  echo "FAIL: y branch was unexpectedly validated in x-only promotion smoke"
  echo "$validate_x"
  exit 1
fi

echo "OK: constraint lattice admission fails until required evidence exists, then any_of promotion succeeds with only x satisfied."
