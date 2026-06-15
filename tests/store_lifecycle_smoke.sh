#!/usr/bin/env bash
# tests/store_lifecycle_smoke.sh
#
# Store lifecycle smoke for no-git workspaces, candidate->patch lifecycle,
# isolated materialize output, refs/graft sync/clone, evidence rebuild, promote, and gc.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
trap cleanup_workspace EXIT

# 1) Graft workspace roots are Git-independent; .git is an explicit boundary.
GIT_WS="$WORKDIR/git-ws"
mkdir -p "$GIT_WS"
git -C "$GIT_WS" init -b main >/dev/null
if "$GRAFT" --cwd "$GIT_WS" init >"$WORKDIR/git-ok.out" 2>"$WORKDIR/git-err.out"; then
  echo "FAIL: graft init unexpectedly succeeded inside a Git worktree"
  exit 1
fi
grep -q "E_GIT_WORKSPACE_UNSUPPORTED" "$WORKDIR/git-err.out" || {
  echo "FAIL: graft init did not report Git workspace rejection"
  cat "$WORKDIR/git-err.out"
  exit 1
}

# 2) Candidate -> validate -> admit moves private objects to public patch/evidence_refs.
WS="$WORKDIR/ws"
mkdir -p "$WS"
cd "$WS"
"$GRAFT" init >/dev/null
write_constraints_roto <<'ROTO'
fn touches_hello(app: Application) -> Constraint {
    primitive(app.changed_paths(["hello.txt"]), any_match, "change touches hello.txt")
}
ROTO
lock_constraints
printf 'hello\n' > hello.txt
scratch_out=$("$GRAFT" scratch write --base graft:empty hello.txt --content $'hello\n')
scratch=$(grep -oE 'scratch:[0-9a-f]+' <<<"$scratch_out" | tail -n1)
[[ -n $scratch ]] || { echo "FAIL: no scratch id"; echo "$scratch_out"; exit 1; }
create=$("$GRAFT" patch from-scratch "$scratch" --expect touches_hello --message store-lifecycle-smoke)
candidate=$(first_graft_id candidate "$create")
[[ -n $candidate ]] || { echo "FAIL: no candidate id"; echo "$create"; exit 1; }
[[ -n $(find .graft/store/private/evidence_refs -type f -print -quit) ]] || { echo "FAIL: from-scratch --expect did not create evidence refs"; echo "$create"; exit 1; }
[[ -n $(find .graft/store/derived/evidence -type f -print -quit) ]] || { echo "FAIL: from-scratch --expect did not create evidence body"; echo "$create"; exit 1; }
"$GRAFT" patch validate "$candidate" --expect touches_hello >/dev/null
admit=$("$GRAFT" patch admit "$candidate" --require touches_hello)
patch=$(first_graft_id patch "$admit")
[[ -n $patch ]] || { echo "FAIL: no patch id"; echo "$admit"; exit 1; }
[[ ! -e ".graft/store/private/candidate/$candidate.json" ]] || { echo "FAIL: private candidate remained after admit"; exit 1; }
[[ ! -e ".graft/store/private/evidence_refs/$candidate.json" ]] || { echo "FAIL: private evidence_refs remained after admit"; exit 1; }
[[ -e ".graft/store/public/patch/$patch.json" ]] || { echo "FAIL: public patch missing"; exit 1; }
[[ -e ".graft/store/public/evidence_refs/$patch.json" ]] || { echo "FAIL: public evidence_refs missing"; exit 1; }
[[ -n $(find .graft/store/derived/evidence -type f -print -quit) ]] || { echo "FAIL: derived evidence body missing before sync"; exit 1; }

# 3) materialize writes an isolated state inspection tree and leaves cwd untouched.
materialize_out=$("$GRAFT" patch materialize "$patch" --discard)
materialized_path=$(extract_materialize_path <<<"$materialize_out")
[[ -n $materialized_path ]] || { echo "FAIL: materialize did not report output path"; echo "$materialize_out"; exit 1; }
[[ "$materialized_path" != *"$patch"* ]] || { echo "FAIL: materialize output path used patch id"; echo "$materialized_path"; exit 1; }
[[ -e "$materialized_path/hello.txt" ]] || { echo "FAIL: materialized worktree missing"; echo "$materialize_out"; exit 1; }
grep -q 'hello' "$materialized_path/hello.txt" || { echo "FAIL: materialized worktree content wrong"; exit 1; }
grep -q 'hello' hello.txt || { echo "FAIL: cwd was unexpectedly modified by materialize"; exit 1; }
if [[ -d .graft/store/public/relation ]] && [[ -n $(find .graft/store/public/relation -type f -print -quit) ]]; then
  echo "FAIL: materialize wrote a registry relation"
  find .graft/store/public/relation -type f
  exit 1
fi

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

required = ["touches_hello"]
TOML
lock_constraints
"$GRAFT" patch promote "$patch" --to out --yes >/dev/null
git -C "$TARGET" rev-parse --verify refs/heads/graft-out >/dev/null
[[ -n $(find .graft/store/public/promotion -type f -print -quit) ]] || { echo "FAIL: promotion record missing"; exit 1; }

# 5) Sync is enabled by default for local workspaces, explicit opt-out fails,
# uses refs/graft/*, and omits derived evidence bodies; clone can rebuild them.
REMOTE="$WORKDIR/remote.git"
sed -i.bak 's/enabled = true/enabled = false/' graft.toml
rm -f graft.toml.bak
set +e
disabled_sync=$("$GRAFT" sync "$REMOTE" --push-only 2>&1)
sync_status=$?
set -e
if [[ $sync_status -eq 0 ]]; then
  echo "FAIL: sync succeeded while [sync] was explicitly disabled"
  echo "$disabled_sync"
  exit 1
fi
grep -q 'E_SYNC_DISABLED' <<<"$disabled_sync" || {
  echo "FAIL: disabled sync did not report E_SYNC_DISABLED"
  echo "$disabled_sync"
  exit 1
}
[[ ! -e "$REMOTE" ]] || { echo "FAIL: disabled sync created remote data"; exit 1; }
sed -i.bak 's/enabled = false/enabled = true/' graft.toml
rm -f graft.toml.bak
"$GRAFT" sync "$REMOTE" --push-only >/dev/null
grep -q "$REMOTE" .graft/local/remotes/default || { echo "FAIL: default sync remote was not recorded"; exit 1; }
"$GRAFT" sync --fetch-only >/dev/null
git --git-dir "$REMOTE" show-ref --verify refs/graft/facts >/dev/null
git --git-dir "$REMOTE" show-ref --verify refs/graft/blobs >/dev/null
git --git-dir "$REMOTE" show-ref --verify refs/graft/manifests >/dev/null
CLONE="$WORKDIR/clone"
"$GRAFT" clone "$REMOTE" "$CLONE" >/dev/null
cp constraints.roto "$CLONE/constraints.roto"
"$GRAFT" --cwd "$CLONE" constraint lock >/dev/null
[[ -e "$CLONE/.graft/store/public/patch/$patch.json" ]] || { echo "FAIL: cloned public patch missing"; exit 1; }
if [[ -n $(find "$CLONE/.graft/store/derived/evidence" -type f -print -quit) ]]; then
  echo "FAIL: clone fetched derived evidence bodies"
  exit 1
fi
incoming=$("$GRAFT" --cwd "$CLONE" incoming)
grep -q 'not locally rebuilt' <<<"$incoming" || { echo "FAIL: incoming did not report pending evidence rebuild"; echo "$incoming"; exit 1; }
verify=$("$GRAFT" --cwd "$CLONE" verify-pending --limit 1)
grep -Eq 'rebuilt|appended|verified|evidence' <<<"$verify" || { echo "FAIL: verify-pending output unexpected"; echo "$verify"; exit 1; }
[[ -n $(find "$CLONE/.graft/store/derived/evidence" -type f -print -quit) ]] || { echo "FAIL: verify-pending did not rebuild derived evidence"; exit 1; }

# 6) GC derived-only dry-run/apply.
gc_dry=$("$GRAFT" --cwd "$CLONE" gc --derived-only)
grep -q 'evidence_bodies_to_delete: 1' <<<"$gc_dry" || { echo "FAIL: gc dry-run did not describe derived evidence"; echo "$gc_dry"; exit 1; }
"$GRAFT" --cwd "$CLONE" gc --derived-only --apply >/dev/null
if [[ -n $(find "$CLONE/.graft/store/derived/evidence" -type f -print -quit) ]]; then
  echo "FAIL: gc --derived-only --apply left evidence bodies"
  exit 1
fi

echo "OK: store lifecycle, view, promote, sync/clone, evidence rebuild, and gc smoke passed."
