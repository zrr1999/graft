#!/usr/bin/env bash
# tests/cli_help_smoke.sh
#
# Verifies that every distinct (subcommand, flag) pair appearing in README.md
# and docs/design.md is actually accepted by the current `graft` CLI. We do
# not invoke the commands for real (no candidates exist
# in many cases); we just ask `graft <subcmd> --help` to parse the flag set.
#
# A drift is anything that produces "unrecognized" / "unknown" / "unexpected
# argument" output, or where `--help` itself fails.
#
# This script is the evidence backing the @clap-about-and-readme-align task.

set -euo pipefail

cd "$(dirname "$0")/.."

cargo build -p graft-cli -p graft-daemon >/dev/null 2>&1
GRAFT="./target/debug/graft"

fail=0
report() {
  local cmd="$1"
  local detail="$2"
  echo "DRIFT  $cmd"
  echo "       $detail"
  fail=$((fail + 1))
}

# Subcommand has-help check: every top-level subcommand must accept --help.
for sub in init clone create candidates show validate admit status diff discard incoming search compose migrate \
           revert materialize promote sync property repo scratch registry cache verify-pending evidence gc learn explain; do
  if ! out=$("$GRAFT" "$sub" --help 2>&1); then
    report "graft $sub --help" "exited non-zero"
    continue
  fi
  if ! grep -qE '^Usage: graft ' <<<"$out"; then
    report "graft $sub --help" "no Usage: line"
  fi
done

# Nested subcommand has-help check.
for nest in "property lock" "property check" "property list" "repo add" "repo list" "repo sync" "repo lock" "repo update" "scratch open" "scratch read" "scratch write" "scratch edit" "scratch promote" "registry export" "registry import" "cache search"; do
  if ! out=$("$GRAFT" $nest --help 2>&1); then
    report "graft $nest --help" "exited non-zero"
  fi
done

# Flag presence check: derived from a hand-curated list of (subcmd, flag)
# pairs that appear in README.md / docs/design.md.
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

check_flag create   "--expect"
check_flag create   "--from"
check_flag create   "--worktree"
check_flag create   "--validate"
check_flag create   "--producer"
check_flag create   "--message"

check_flag candidates   "--property"
check_flag candidates   "--failed"
check_flag candidates   "--producer"
check_flag show     "--evidence"
check_flag show     "--change"
check_flag validate "--expect"
check_flag admit    "--require"
check_flag search   "--property"
check_flag search   "--base"
check_flag search   "--producer"
check_flag search   "--has-evidence"
check_flag compose  "--expect"
check_flag compose  "--validate"
check_flag migrate  "--onto"
check_flag migrate  "--expect"
check_flag migrate  "--validate"
check_flag revert   "--expect"
check_flag revert   "--validate"
check_flag materialize "--dry-run"
check_flag materialize "--discard"
check_flag materialize "--as-commit"
check_flag materialize "--ref"
check_flag promote  "--to"
check_flag promote  "--branch"
check_flag promote  "--yes"
check_flag promote  "--require"
check_flag promote  "--pr"
check_flag promote  "--release"
check_flag sync "--fetch-only"
check_flag sync "--push-only"
check_flag verify-pending "--patch"
check_flag verify-pending "--limit"
check_flag gc "--apply"
check_flag gc "--derived-only"
check_flag "cache search" "--property"
check_flag "cache search" "--failed"

if [[ $fail -gt 0 ]]; then
  echo
  echo "FAILED: $fail drift(s) detected"
  exit 1
fi

echo
echo "OK: every README/docs/design.md (subcommand, flag) pair is accepted by the current CLI"
