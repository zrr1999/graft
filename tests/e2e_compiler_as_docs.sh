#!/usr/bin/env bash
# tests/e2e_compiler_as_docs.sh
#
# End-to-end smoke for the compiler-as-documentation thread: help/about text,
# learn, diagnostic codes, property warnings, explain lookup, Hole Report, and
# promotion policy source all come from the implemented structural layers.

set -euo pipefail

cd "$(dirname "$0")/.."

cargo build -p graft-cli -p graft-daemon >/dev/null 2>&1
GRAFT_BIN="$PWD/target/debug/graft"
GRAFTD_BIN="$PWD/target/debug/graftd"

help=$("$GRAFT_BIN" --help)
if ! grep -q 'Capture, validate and admit patch candidates' <<<"$help"; then
  echo "FAIL: top-level help missing long_about text"
  echo "$help"; exit 1
fi
if ! grep -q 'learn' <<<"$help" || ! grep -q 'explain' <<<"$help"; then
  echo "FAIL: top-level help missing learn/explain commands"
  echo "$help"; exit 1
fi

learn=$("$GRAFT_BIN" learn --non-interactive)
if ! grep -q 'learn complete:' <<<"$learn"; then
  echo "FAIL: learn non-interactive did not complete"
  echo "$learn"; exit 1
fi
if ! grep -qE '^step: promote$' <<<"$learn"; then
  echo "FAIL: learn output did not reach promote step"
  echo "$learn"; exit 1
fi

NOGIT="$(mktemp -d)"
PROMOTE="$(mktemp -d)"
cleanup() {
  find "$NOGIT" "$PROMOTE" -path '*/.graft/run/daemon.sock' -type s -exec "$GRAFTD_BIN" stop --socket {} \; >/dev/null 2>&1 || true
  rm -rf "$NOGIT" "$PROMOTE"
}
trap cleanup EXIT

cd "$NOGIT"
printf 'nogit\n' > hello.txt
"$GRAFT_BIN" init >/dev/null

# 1) `create` without an explicit base in a no-git workspace must fail loud
#    with a structured B001 diagnostic, not by silently inventing an
#    `unknown-head-tree` placeholder.
if "$GRAFT_BIN" create --expect ValidPatch --message nogit >/tmp/nogit-out 2>/tmp/nogit-err; then
  echo "FAIL: create unexpectedly succeeded in no-git workspace without --from graft:empty"
  cat /tmp/nogit-out; exit 1
fi
if ! grep -q '\[B001\]' /tmp/nogit-err; then
  echo "FAIL: no-git create did not surface B001"
  cat /tmp/nogit-err; exit 1
fi

# 2) `--from graft:empty` is the documented escape hatch for no-git contexts;
#    it must produce a real candidate whose ValidPatch evidence passes.
create=$("$GRAFT_BIN" create --from graft:empty --expect ValidPatch --validate --message nogit-empty)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$create" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no candidate from --from graft:empty create"; echo "$create"; exit 1; }
if ! grep -qE '^next:$' <<<"$create" || ! grep -q '\[recommended\]' <<<"$create"; then
  echo "FAIL: create output missing Hole Report"
  echo "$create"; exit 1
fi
search=$("$GRAFT_BIN" search --property TestsPass 2>&1 >/dev/null || true)
if ! grep -q 'property `TestsPass` is not declared' <<<"$search"; then
  echo "FAIL: search unknown property warning missing"
  echo "$search"; exit 1
fi

for spec in 'admit:^concept: admit$' 'V003:^diagnostic: V003$' 'ValidPatch:^builtin property: valid_patch$'; do
  id=${spec%%:*}
  pattern=${spec#*:}
  out=$("$GRAFT_BIN" explain "$id")
  if ! grep -qE "$pattern" <<<"$out"; then
    echo "FAIL: explain $id did not match $pattern"
    echo "$out"; exit 1
  fi
done

cd "$PROMOTE"
"$GRAFT_BIN" init >/dev/null
printf 'changed
' > hello.txt
TARGET="$PROMOTE-target"
mkdir -p "$TARGET"
git -C "$TARGET" init -b main >/dev/null
git -C "$TARGET" config user.email smoke@example.invalid
git -C "$TARGET" config user.name "Graft Smoke"
git -C "$TARGET" config commit.gpgsign false
cat >> graft.toml <<TOML

[promote_targets.main]
path = "$TARGET"
branch = "graft-out"
required_properties = ["ValidPatch"]
TOML
created=$("$GRAFT_BIN" create --from graft:empty --expect ValidPatch --message policy-source)
created_candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$created" | head -n1)
"$GRAFT_BIN" validate "$created_candidate" >/dev/null
admitted=$("$GRAFT_BIN" admit "$created_candidate" --require ValidPatch)
patch=$(grep -oE 'patch:[0-9a-f]+' <<<"$admitted" | head -n1)
promote=$("$GRAFT_BIN" promote "$patch" --to main 2>&1)
if ! grep -q 'source: config' <<<"$promote"; then
  echo "FAIL: promote should report config requirement source"
  echo "$promote"; exit 1
fi
if grep -q 'legacy default Builds,TestsPass' <<<"$promote"; then
  echo "FAIL: promote should not use legacy fallback"
  echo "$promote"; exit 1
fi

echo "OK: compiler-as-docs e2e covers help, learn, V003, Hole Report, warnings, explain, and promote policy source."
