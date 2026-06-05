#!/usr/bin/env bash
# tests/default_workspace_sync_smoke.sh
#
# Unattached cwd must not silently fall back to ws:default. An explicit
# ws:default attach is still machine-local scratch space and must not sync.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
trap cleanup_workspace EXIT
require_local_socket_bind

UNATTACHED="$WORKDIR/unattached"
REMOTE="$WORKDIR/remote.git"
mkdir -p "$UNATTACHED"

workspace_status=$("$GRAFT" --cwd "$UNATTACHED" workspace status)
grep -q $'workspace\t<none>' <<<"$workspace_status" || {
  echo "FAIL: unattached workspace status did not report no resolved workspace"
  echo "$workspace_status"
  exit 1
}
grep -q $'route\t<none>' <<<"$workspace_status" || {
  echo "FAIL: unattached workspace status did not report missing route"
  echo "$workspace_status"
  exit 1
}
grep -q $'daemon_state\tmissing' <<<"$workspace_status" || {
  echo "FAIL: unattached workspace status did not report missing daemon without spawning it"
  echo "$workspace_status"
  exit 1
}

legacy_status=$("$GRAFT" --cwd "$UNATTACHED" status)
grep -q $'workspace\t<none>' <<<"$legacy_status" || {
  echo "FAIL: unattached legacy status did not report no resolved workspace"
  echo "$legacy_status"
  exit 1
}
grep -q $'route\t<none>' <<<"$legacy_status" || {
  echo "FAIL: unattached legacy status did not report missing route"
  echo "$legacy_status"
  exit 1
}
grep -q $'daemon_state\tmissing' <<<"$legacy_status" || {
  echo "FAIL: unattached legacy status did not report missing daemon without spawning it"
  echo "$legacy_status"
  exit 1
}

set +e
sync_out=$("$GRAFT" --cwd "$UNATTACHED" sync "$REMOTE" --push-only 2>&1)
status=$?
set -e

if [[ $status -eq 0 ]]; then
  echo "FAIL: unattached cwd sync unexpectedly succeeded"
  echo "$sync_out"
  exit 1
fi
grep -q 'E_NO_WORKSPACE' <<<"$sync_out" || {
  echo "FAIL: unattached cwd sync did not report E_NO_WORKSPACE"
  echo "$sync_out"
  exit 1
}
if [[ -e "$REMOTE/graft-public" ]]; then
  echo "FAIL: unattached cwd sync wrote remote public data"
  exit 1
fi

"$GRAFT" --cwd "$UNATTACHED" attach >/dev/null
property_check=$("$GRAFT" --cwd "$UNATTACHED" property check)
grep -q 'property lock current' <<<"$property_check" || {
  echo "FAIL: explicit default workspace is missing a usable property lock"
  echo "$property_check"
  exit 1
}
set +e
default_sync_out=$("$GRAFT" --cwd "$UNATTACHED" sync "$REMOTE" --push-only 2>&1)
default_status=$?
set -e

if [[ $default_status -eq 0 ]]; then
  echo "FAIL: explicit ws:default sync unexpectedly succeeded"
  echo "$default_sync_out"
  exit 1
fi
grep -q 'E_SYNC_DEFAULT_WORKSPACE' <<<"$default_sync_out" || {
  echo "FAIL: explicit default workspace sync did not report E_SYNC_DEFAULT_WORKSPACE"
  echo "$default_sync_out"
  exit 1
}
if [[ -e "$REMOTE/graft-public" ]]; then
  echo "FAIL: explicit default workspace sync wrote remote public data"
  exit 1
fi

echo "OK: unattached cwd fails loud, and explicit ws:default refuses graft sync."
