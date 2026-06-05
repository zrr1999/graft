#!/usr/bin/env bash
# tests/property_warning_smoke.sh
#
# Verifies that `graft search`/`graft candidates`/`graft cache search` emit a
# warning to stderr when --property names something that is not declared in
# properties.roto. Builtin evaluator ids are not property aliases.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
trap cleanup_workspace EXIT

cd "$WORKDIR"
"$GRAFT_BIN" init >/dev/null
write_properties_roto <<'ROTO'
fn empty_change(app: Application) -> Property {
    property(
        [
            app.changed_paths().any_match(["**"]).failure(),
        ],
        "the change touches no paths",
        Severity.Blocking,
        [],
    )
}
ROTO
lock_properties

expect_warn() {
  local desc="$1"; shift
  local err
  err=$("$GRAFT_BIN" "$@" 2>&1 >/dev/null) || true
  if ! grep -q "warning: property" <<<"$err"; then
    echo "FAIL: ($desc) expected warning to stderr"
    echo "----"
    echo "$err"
    exit 1
  fi
  if ! grep -q "graft property list" <<<"$err"; then
    echo "FAIL: ($desc) warning should hint at graft property list"
    exit 1
  fi
}

expect_silent() {
  local desc="$1"; shift
  local err
  err=$("$GRAFT_BIN" "$@" 2>&1 >/dev/null) || true
  if grep -q "warning: property" <<<"$err"; then
    echo "FAIL: ($desc) unexpected warning to stderr"
    echo "----"
    echo "$err"
    exit 1
  fi
}

# Unknown property: warn from search, candidates, and cache search.
expect_warn "search/unknown"        search --property TestsPass
expect_warn "candidates/unknown"        candidates --property TestsPass
expect_warn "cache-search/unknown"  cache search --property TestsPass

# Declared property: no warning.
expect_silent "search/declared"        search --property empty_change
expect_silent "candidates/declared"        candidates --property empty_change
expect_silent "cache-search/declared"  cache search --property empty_change

# Builtin evaluator id is not a property alias: warn.
expect_warn "search/builtin-evaluator-id"        search --property changed_paths_any_match
expect_warn "candidates/builtin-evaluator-id"        candidates --property changed_paths_any_match
expect_warn "cache-search/builtin-evaluator-id"  cache search --property changed_paths_any_match

echo "OK: property warnings fire for unknown names and builtin evaluator ids; declared properties stay silent."
