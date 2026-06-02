#!/usr/bin/env bash
# tests/v2_store_tier_smoke.sh
#
# End-to-end v2 smoke for no-git workspaces, candidate->patch lifecycle,
# cwd view gates, refs/graft sync/clone, evidence rebuild, promote, and gc.

set -euo pipefail

cd "$(dirname "$0")/.."

cargo build -p graft-cli -p graft-daemon >/dev/null 2>&1

GRAFT="$PWD/target/debug/graft"
GRAFTD="$PWD/target/debug/graftd"

WORKDIR="$(mktemp -d)"
cleanup() {
  find "$WORKDIR" -path '*/.graft/run/daemon.sock' -type s -exec "$GRAFTD" stop --socket {} \; >/dev/null 2>&1 || true
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

# 1) cwd root must not be a Git worktree.
GIT_WS="$WORKDIR/git-ws"
mkdir -p "$GIT_WS"
git -C "$GIT_WS" init -b main >/dev/null
if "$GRAFT" --cwd "$GIT_WS" init >git-ok.out 2>git-err.out; then
  echo "FAIL: graft init succeeded inside a Git worktree"
  cat git-ok.out
  exit 1
fi
grep -q '\[E_GIT_IN_WORKSPACE\]' git-err.out || { echo "FAIL: no E_GIT_IN_WORKSPACE"; cat git-err.out; exit 1; }

# 2) Candidate -> validate -> admit moves private objects to public patch/evidence_refs.
WS="$WORKDIR/ws"
mkdir -p "$WS"
cd "$WS"
"$GRAFT" init >/dev/null
printf 'hello\n' > hello.txt
create=$("$GRAFT" create --from graft:empty --expect ValidPatch --message v2-smoke)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$create" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no candidate id"; echo "$create"; exit 1; }
"$GRAFT" validate "$candidate" --expect ValidPatch >/dev/null
admit=$("$GRAFT" admit "$candidate" --require ValidPatch)
patch=$(grep -oE 'patch:[0-9a-f]+' <<<"$admit" | head -n1)
[[ -n $patch ]] || { echo "FAIL: no patch id"; echo "$admit"; exit 1; }
[[ ! -e ".graft/store/private/candidate/$candidate.json" ]] || { echo "FAIL: private candidate remained after admit"; exit 1; }
[[ ! -e ".graft/store/private/evidence_refs/$candidate.json" ]] || { echo "FAIL: private evidence_refs remained after admit"; exit 1; }
[[ -e ".graft/store/public/patch/$patch.json" ]] || { echo "FAIL: public patch missing"; exit 1; }
[[ -e ".graft/store/public/evidence_refs/$patch.json" ]] || { echo "FAIL: public evidence_refs missing"; exit 1; }
find .graft/store/derived/evidence -type f | grep -q . || { echo "FAIL: derived evidence body missing before sync"; exit 1; }

# 3) cwd view dirty gate and discard.
"$GRAFT" materialize "$patch" --discard >/dev/null
status=$("$GRAFT" status)
grep -q 'cwd clean' <<<"$status" || { echo "FAIL: materialized cwd is not clean"; echo "$status"; exit 1; }
printf 'changed\n' > hello.txt
diff=$("$GRAFT" diff)
grep -q 'cwd dirty' <<<"$diff" || { echo "FAIL: diff did not report dirty cwd"; echo "$diff"; exit 1; }
"$GRAFT" discard >/dev/null
grep -q 'hello' hello.txt || { echo "FAIL: discard did not restore materialized view"; exit 1; }

# 4) Promote to a configured external Git target and record promotion object.
TARGET="$WORKDIR/target-git"
mkdir -p "$TARGET"
git -C "$TARGET" init -b main >/dev/null
git -C "$TARGET" config user.email smoke@example.invalid
git -C "$TARGET" config user.name "Graft Smoke"
git -C "$TARGET" config commit.gpgsign false
cat >> graft.toml <<TOML

[promote_targets.out]
path = "$TARGET"
branch = "graft-out"
required_properties = ["ValidPatch"]
TOML
"$GRAFT" promote "$patch" --to out --yes >/dev/null
git -C "$TARGET" rev-parse --verify refs/heads/graft-out >/dev/null
find .graft/store/public/promotion -type f | grep -q . || { echo "FAIL: promotion record missing"; exit 1; }

# 5) Sync uses refs/graft/* and omits derived evidence bodies; clone can rebuild them.
REMOTE="$WORKDIR/remote.git"
"$GRAFT" sync "$REMOTE" --push-only >/dev/null
git --git-dir "$REMOTE" show-ref --verify refs/graft/facts >/dev/null
git --git-dir "$REMOTE" show-ref --verify refs/graft/blobs >/dev/null
git --git-dir "$REMOTE" show-ref --verify refs/graft/manifests >/dev/null
CLONE="$WORKDIR/clone"
"$GRAFT" clone "$REMOTE" "$CLONE" >/dev/null
[[ -e "$CLONE/.graft/store/public/patch/$patch.json" ]] || { echo "FAIL: cloned public patch missing"; exit 1; }
if find "$CLONE/.graft/store/derived/evidence" -type f | grep -q .; then
  echo "FAIL: clone fetched derived evidence bodies"
  exit 1
fi
incoming=$("$GRAFT" --cwd "$CLONE" incoming)
grep -q 'not locally rebuilt' <<<"$incoming" || { echo "FAIL: incoming did not report pending evidence rebuild"; echo "$incoming"; exit 1; }
verify=$("$GRAFT" --cwd "$CLONE" verify-pending --limit 1)
grep -Eq 'rebuilt|appended|verified|evidence' <<<"$verify" || { echo "FAIL: verify-pending output unexpected"; echo "$verify"; exit 1; }
find "$CLONE/.graft/store/derived/evidence" -type f | grep -q . || { echo "FAIL: verify-pending did not rebuild derived evidence"; exit 1; }

# 6) GC derived-only dry-run/apply.
gc_dry=$("$GRAFT" --cwd "$CLONE" gc --derived-only)
grep -q 'derived-only would delete' <<<"$gc_dry" || { echo "FAIL: gc dry-run did not describe derived evidence"; echo "$gc_dry"; exit 1; }
"$GRAFT" --cwd "$CLONE" gc --derived-only --apply >/dev/null
if find "$CLONE/.graft/store/derived/evidence" -type f | grep -q .; then
  echo "FAIL: gc --derived-only --apply left evidence bodies"
  exit 1
fi

echo "OK: v2 store-tier lifecycle, view, promote, sync/clone, evidence rebuild, and gc smoke passed."
