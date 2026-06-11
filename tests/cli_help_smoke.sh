#!/usr/bin/env bash
# tests/cli_help_smoke.sh
#
# Verifies that the documented CLI surface stays aligned with the current
# top-level command router. Canonical docs should use the visible grouped
# commands (`workspace`, `patch`, `bundle`, ...); patch lifecycle commands live
# under `graft patch ...`, while `graft run` remains top-level.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins

fail=0
report() {
  local cmd="$1"
  local detail="$2"
  echo "DRIFT  $cmd"
  echo "       $detail"
  fail=$((fail + 1))
}

check_help() {
  local cmd="$1"
  local out
  if ! out=$("$GRAFT" $cmd --help 2>&1); then
    report "graft $cmd --help" "exited non-zero"
    return
  fi
  if ! grep -qE '^Usage: graft ' <<<"$out"; then
    report "graft $cmd --help" "no Usage: line"
  fi
}

# Visible top-level command groups.
for sub in get sync workspace scratch patch repo bundle explain run; do
  check_help "$sub"
done

# Hidden compatibility aliases still parse for existing automation, but the
# canonical docs/help should prefer the grouped forms checked below.
for sub in init clone candidate candidates show validate admit status diff discard incoming search compose migrate \
           revert materialize promote property registry cache verify-pending evidence gc; do
  check_help "$sub"
done

# Canonical nested command help checks.
for nest in \
  "workspace init" "workspace status" "workspace attach" "workspace detach" "workspace ps" "workspace doctor" "workspace gc" \
  "patch list" "patch from-scratch" "patch show" "patch validate" "patch admit" "patch incoming" "patch search" \
  "patch diff" "patch compose" "patch migrate" "patch revert" "patch materialize" "patch promote" \
  "property lock" "property check" "property list" "property show" \
  "repo add" "repo list" "repo sync" "repo lock" "repo update" \
  "scratch status" "scratch read" "scratch write" "scratch edit" "scratch delete" "scratch rm" \
  "scratch capture" "scratch diff" "scratch drop" "scratch pin" "scratch unpin" \
  "bundle export" "bundle import" "registry export" "registry import" "cache search" "candidate from-scratch"; do
  check_help "$nest"
done

top_help=$("$GRAFT" --help 2>&1) || report "graft --help" "exited non-zero"
for hidden_top in init clone candidate candidates show validate admit status diff discard incoming search compose migrate \
                  revert materialize promote property registry cache verify-pending evidence gc create learn; do
  if grep -qE "^[[:space:]]+${hidden_top}[[:space:]]" <<<"$top_help"; then
    report "graft --help" "hidden/removed top-level command must not be user-facing: $hidden_top"
  fi
done
for removed_top in create learn; do
  if "$GRAFT" "$removed_top" --help >/tmp/graft-removed-top-help.out 2>&1; then
    report "graft $removed_top --help" "removed top-level command unexpectedly parsed"
  fi
done

scratch_help=$("$GRAFT" scratch --help 2>&1) || report "graft scratch --help" "exited non-zero"
if grep -qE '^[[:space:]]+(open|promote)[[:space:]]' <<<"$scratch_help"; then
  report "graft scratch --help" "scratch open/promote must not be user-facing scratch commands"
fi
for removed in "scratch open" "scratch promote"; do
  if "$GRAFT" $removed --help >/tmp/graft-removed-help.out 2>&1; then
    report "graft $removed --help" "removed scratch subcommand unexpectedly parsed"
  fi
done

repo_add_help=$("$GRAFT" repo add --help 2>&1) || report "graft repo add --help" "exited non-zero"
if grep -q -- '--path' <<<"$repo_add_help"; then
  report "graft repo add --help" "repo add must use Graft's default repo cache, not expose --path"
fi
if grep -q -- '--no-auto-clone' <<<"$repo_add_help"; then
  report "graft repo add --help" "repo add auto-locks, so --no-auto-clone must not be user-facing"
fi

# Hidden compatibility no-op: accepted for old scripts, intentionally absent
# from normal help output.
if ! "$GRAFT" patch materialize --discard --help >/tmp/graft-materialize-discard-help.out 2>&1; then
  report "graft patch materialize --discard --help" "hidden --discard compatibility flag no longer parses"
fi
if grep -q -- '--discard' <<<"$("$GRAFT" patch materialize --help 2>&1)"; then
  report "graft patch materialize --help" "hidden --discard compatibility flag leaked into help"
fi
for hidden_materialize_flag in '--as-commit' '--ref'; do
  if grep -q -- "$hidden_materialize_flag" <<<"$("$GRAFT" patch materialize --help 2>&1)"; then
    report "graft patch materialize --help" "unsupported Git flag leaked into help: $hidden_materialize_flag"
  fi
done

# Flag presence check: curated from README.md / docs/design.md command references.
check_flag() {
  local sub="$1"
  local flag="$2"
  local help
  help=$("$GRAFT" $sub --help 2>&1) || {
    report "graft $sub --help" "help failed before flag check"
    return
  }
  if ! grep -qE "^[[:space:]]+$flag([[:space:]<,]|$)" <<<"$help"; then
    report "graft $sub" "doc references $flag, but --help does not list it"
  fi
}

check_help_contains() {
  local sub="$1"
  local text="$2"
  local help
  help=$("$GRAFT" $sub --help 2>&1) || {
    report "graft $sub --help" "help failed before text check"
    return
  }
  if ! grep -Fq "$text" <<<"$help"; then
    report "graft $sub --help" "help must explain: $text"
  fi
}

check_flag "workspace init" "--register-only"
check_flag "workspace attach" "--workspace"
check_flag "workspace attach" "--status"
check_flag "workspace doctor" "--rebuild-registry"
check_flag "workspace gc" "--apply"
check_flag "workspace gc" "--derived-only"
check_flag "patch from-scratch" "--expect"
check_flag "patch from-scratch" "--producer"
check_flag "patch from-scratch" "--message"
check_flag "patch list" "--candidates"
check_flag "patch list" "--all"
check_flag "patch list" "--property"
check_flag "patch list" "--producer"
check_flag "patch show" "--evidence"
check_flag "patch show" "--change"
check_flag "patch validate" "--expect"
check_flag "patch admit" "--require"
check_help_contains "patch admit" "one-shot admission requirement"
check_help_contains "patch admit" "append to [admission.required_properties]"
check_flag "patch search" "--property"
check_flag "patch search" "--base"
check_flag "patch search" "--producer"
check_flag "patch search" "--has-evidence"
check_flag "patch compose" "--expect"
check_flag "patch compose" "--validate"
check_flag "patch migrate" "--onto"
check_flag "patch migrate" "--expect"
check_flag "patch migrate" "--validate"
check_flag "patch revert" "--expect"
check_flag "patch revert" "--validate"
check_flag "patch materialize" "--dry-run"
check_flag "run" "--cwd"
check_flag "patch promote" "--to"
check_flag "patch promote" "--branch"
check_flag "patch promote" "--yes"
check_flag "patch promote" "--require"
check_flag "patch promote" "--pr"
check_flag "patch promote" "--release"
check_flag "sync" "--fetch-only"
check_flag "sync" "--push-only"
check_flag "sync" "--on-divergence"
check_flag "verify-pending" "--patch"
check_flag "verify-pending" "--limit"
check_flag "scratch read" "--base"
check_flag "scratch read" "--from"
check_flag "scratch read" "--repo"
check_flag "scratch read" "--mode"
check_flag "scratch write" "--base"
check_flag "scratch write" "--from"
check_flag "scratch write" "--repo"
check_flag "scratch write" "--content"
check_flag "scratch edit" "--base"
check_flag "scratch edit" "--from"
check_flag "scratch edit" "--repo"
check_flag "scratch edit" "--edits"
check_flag "scratch delete" "--base"
check_flag "scratch delete" "--from"
check_flag "scratch delete" "--repo"
check_flag "scratch capture" "--base"
check_flag "scratch capture" "--repo"
check_flag "scratch capture" "--dry-run"
check_flag "cache search" "--property"
check_flag "cache search" "--failed"

if [[ $fail -gt 0 ]]; then
  echo
  echo "FAILED: $fail drift(s) detected"
  exit 1
fi

echo
echo "OK: README/docs/design.md CLI references match visible command groups and hidden compatibility expectations"
