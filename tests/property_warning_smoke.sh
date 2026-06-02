#!/usr/bin/env bash
# tests/property_warning_smoke.sh
#
# Verifies that `graft search`/`graft candidates`/`graft cache search` emit a
# warning to stderr when --property names something that is neither declared
# in graft-properties.toml nor a builtin verifier id, but stay silent when --property
# matches either of those. Backs the @builtin-property-metadata task.

set -euo pipefail

cd "$(dirname "$0")/.."

cargo build -p graft-cli -p graft-daemon >/dev/null 2>&1
GRAFT_BIN="$PWD/target/debug/graft"
GRAFTD_BIN="$PWD/target/debug/graftd"

WORKDIR="$(mktemp -d)"
cleanup() {
  find "$WORKDIR" -path '*/.graft/run/daemon.sock' -type s -exec "$GRAFTD_BIN" stop --socket {} \; >/dev/null 2>&1 || true
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

cd "$WORKDIR"
"$GRAFT_BIN" init >/dev/null

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

# Declared property (in graft-properties.toml shipped with `graft init`): no warning.
expect_silent "search/declared"        search --property ValidPatch
expect_silent "candidates/declared"        candidates --property ValidPatch
expect_silent "cache-search/declared"  cache search --property ValidPatch

# Builtin id (lowercase check name): no warning.
expect_silent "search/builtin-id"        search --property valid_patch
expect_silent "candidates/builtin-id"        candidates --property valid_patch
expect_silent "cache-search/builtin-id"  cache search --property valid_patch

echo "OK: property warnings fire only for unknown names; declared and builtin ids stay silent."
