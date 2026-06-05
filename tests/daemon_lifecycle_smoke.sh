#!/usr/bin/env bash
# tests/daemon_lifecycle_smoke.sh
#
# Pins daemon ownership and cleanup for v2.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
PROJECT="$WORKDIR/project"
OTHER_SOCKET="$GRAFT_HOME/run/daemon-other.sock"
mkdir -p "$PROJECT"
trap cleanup_workspace EXIT
require_local_socket_bind

cd "$PROJECT"
"$GRAFT" init >/dev/null
cat >> .graft/config.toml <<'TOML'
[daemon]
idle_timeout_seconds = 10
TOML

daemon_help=$("$GRAFTD" --help)
grep -q '\$GRAFT_HOME/run/daemon.sock' <<<"$daemon_help" || { echo "FAIL: graftd help does not document global run socket"; exit 1; }
! grep -q '\.graft/\.lock' <<<"$daemon_help" || { echo "FAIL: graftd help still documents legacy .graft/.lock"; exit 1; }

"$GRAFTD" start --cwd "$PROJECT"
[[ -S "$SOCKET" ]] || { echo "FAIL: default daemon socket did not use GRAFT_HOME/run/daemon.sock"; exit 1; }
default_status=$("$GRAFTD" status)
echo "$default_status" | grep -q '"status":"ok"' || { echo "FAIL: default daemon status did not use GRAFT_HOME/run/daemon.sock: $default_status"; exit 1; }
"$GRAFTD" stop >/dev/null 2>&1 || true
for _ in {1..50}; do
  [[ ! -S "$SOCKET" ]] && break
  sleep 0.1
done
[[ ! -S "$SOCKET" ]] || { echo "FAIL: default daemon socket lingered after stop"; exit 1; }
[[ ! -f "$GRAFT_HOME/run/daemon.pid" ]] || { echo "FAIL: default daemon pid lingered after stop"; exit 1; }

mkdir -p .graft/run/tmp/stale .graft/run/trials/stale .graft/run/worktrees/stale
touch .graft/run/tmp/stale/file .graft/run/trials/stale/file .graft/run/worktrees/stale/file

start_started=$(date +%s)
"$GRAFTD" start --cwd "$PROJECT" --socket "$SOCKET"
start_finished=$(date +%s)
elapsed=$(( start_finished - start_started ))
[[ $elapsed -le 5 ]] || { echo "FAIL: graftd start took ${elapsed}s, expected <=5"; exit 1; }
[[ -S "$SOCKET" ]] || { echo "FAIL: socket missing after start"; exit 1; }
[[ -f "$GRAFT_HOME/run/daemon.pid" ]] || { echo "FAIL: pid file missing after start"; exit 1; }
[[ ! -f "$PROJECT/.graft/.lock" ]] || { echo "FAIL: daemon created legacy .graft/.lock"; exit 1; }
[[ ! -e .graft/run/tmp/stale/file ]] || { echo "FAIL: run/tmp was not cleaned on daemon start"; exit 1; }
[[ ! -e .graft/run/trials/stale/file ]] || { echo "FAIL: run/trials was not cleaned on daemon start"; exit 1; }
[[ ! -e .graft/run/worktrees/stale/file ]] || { echo "FAIL: run/worktrees was not cleaned on daemon start"; exit 1; }

status_out=$("$GRAFTD" status --socket "$SOCKET")
echo "$status_out" | grep -q '"status":"ok"' || { echo "FAIL: status did not return ok: $status_out"; exit 1; }

"$GRAFTD" start --cwd "$PROJECT" --socket "$SOCKET"
new_status=$("$GRAFTD" status --socket "$SOCKET")
echo "$new_status" | grep -q '"status":"ok"' || { echo "FAIL: status broken after idempotent start"; exit 1; }

"$GRAFTD" stop --socket "$SOCKET" >/dev/null 2>&1 || true
for _ in {1..50}; do
  [[ ! -S "$SOCKET" ]] && break
  sleep 0.1
done
[[ ! -S "$SOCKET" ]] || { echo "FAIL: socket lingered after stop"; exit 1; }
[[ ! -f "$GRAFT_HOME/run/daemon.pid" ]] || { echo "FAIL: pid file lingered after stop"; exit 1; }

python3 -c 'from pathlib import Path; p=Path(".graft/config.toml"); p.write_text(p.read_text().replace("idle_timeout_seconds = 10", "idle_timeout_seconds = 1"))'
"$GRAFTD" start --cwd "$PROJECT" --socket "$SOCKET"
[[ -S "$SOCKET" ]] || { echo "FAIL: restart did not bring up socket"; exit 1; }
for _ in {1..50}; do
  [[ ! -S "$SOCKET" ]] && break
  sleep 0.1
done
[[ ! -S "$SOCKET" ]] || { echo "FAIL: idle timeout did not remove socket"; exit 1; }
[[ ! -f "$GRAFT_HOME/run/daemon.pid" ]] || { echo "FAIL: idle timeout did not remove pid file"; exit 1; }

cat > .graft/config.toml <<'TOML'
[daemon]
idle_timeout_seconds = "fast"
TOML
set +e
bad_config_out=$("$GRAFTD" start --cwd "$PROJECT" --socket "$SOCKET" 2>&1)
bad_config_status=$?
set -e
[[ $bad_config_status -ne 0 ]] || { echo "FAIL: graftd start accepted invalid daemon config"; exit 1; }
grep -q 'parse daemon config' <<<"$bad_config_out" || { echo "FAIL: invalid daemon config error was not surfaced"; echo "$bad_config_out"; exit 1; }
[[ ! -S "$SOCKET" ]] || { echo "FAIL: invalid daemon config still created socket"; exit 1; }
set +e
bad_serve_out=$("$GRAFTD" serve --cwd "$PROJECT" --socket "$OTHER_SOCKET" 2>&1)
bad_serve_status=$?
set -e
[[ $bad_serve_status -ne 0 ]] || { echo "FAIL: graftd serve accepted invalid daemon config"; exit 1; }
grep -q 'parse daemon config' <<<"$bad_serve_out" || { echo "FAIL: invalid daemon config serve error was not surfaced"; echo "$bad_serve_out"; exit 1; }
[[ ! -S "$OTHER_SOCKET" ]] || { echo "FAIL: invalid daemon config serve still created socket"; exit 1; }

echo "OK: graftd lifecycle, PID ownership, run-dir cleanup, no .lock, idle timeout, and config validation are intact."
