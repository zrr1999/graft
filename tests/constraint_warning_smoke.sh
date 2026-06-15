#!/usr/bin/env bash
# tests/constraint_warning_smoke.sh
#
# Verifies that `graft search`/`graft candidates`/`graft cache search` emit a
# warning to stderr when --constraint names something that is not declared in
# constraints.roto. Builtin evaluator ids are not constraint aliases.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
trap cleanup_workspace EXIT

cd "$WORKDIR"
"$GRAFT_BIN" init >/dev/null
write_constraints_roto <<'ROTO'
fn empty_change(app: Application) -> Constraint {
    primitive(app.changed_paths(["**"]), no_match, "the change touches no paths")
}
ROTO
lock_constraints

expect_warn() {
  local desc="$1"; shift
  local err
  err=$("$GRAFT_BIN" "$@" 2>&1 >/dev/null) || true
  if ! grep -q "warning: constraint" <<<"$err"; then
    echo "FAIL: ($desc) expected warning to stderr"
    echo "----"
    echo "$err"
    exit 1
  fi
  if ! grep -q "graft constraint list" <<<"$err"; then
    echo "FAIL: ($desc) warning should hint at graft constraint list"
    exit 1
  fi
}

expect_silent() {
  local desc="$1"; shift
  local err
  err=$("$GRAFT_BIN" "$@" 2>&1 >/dev/null) || true
  if grep -q "warning: constraint" <<<"$err"; then
    echo "FAIL: ($desc) unexpected warning to stderr"
    echo "----"
    echo "$err"
    exit 1
  fi
}

# Unknown constraint: warn from search, candidates, and cache search.
expect_warn "search/unknown"        search --constraint TestsPass
expect_warn "candidates/unknown"        candidates --constraint TestsPass
expect_warn "cache-search/unknown"  cache search --constraint TestsPass

# Declared constraint: no warning.
expect_silent "search/declared"        search --constraint empty_change
expect_silent "candidates/declared"        candidates --constraint empty_change
expect_silent "cache-search/declared"  cache search --constraint empty_change

# Builtin evaluator id is not a constraint alias: warn.
expect_warn "search/builtin-evaluator-id"        search --constraint changed_paths_any_match
expect_warn "candidates/builtin-evaluator-id"        candidates --constraint changed_paths_any_match
expect_warn "cache-search/builtin-evaluator-id"  cache search --constraint changed_paths_any_match

echo "OK: constraint warnings fire for unknown names and builtin evaluator ids; declared constraints stay silent."
