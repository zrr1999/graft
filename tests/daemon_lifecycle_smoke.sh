#!/usr/bin/env bash
# tests/daemon_lifecycle_smoke.sh
#
# Pins daemon ownership and cleanup for v2.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/socket_probe.sh

cargo build -p graft-cli -p graft-daemon >/dev/null 2>&1

GRAFT="$PWD/target/debug/graft"
GRAFTD="$PWD/target/debug/graftd"

WORKDIR="$(mktemp -d)"
PROJECT="$WORKDIR/project"
SOCKET="$PROJECT/.graft/run/daemon.sock"
OTHER_SOCKET="$PROJECT/.graft/run/daemon-other.sock"
mkdir -p "$PROJECT"
trap 'if [[ -S "$SOCKET" ]]; then "$GRAFTD" stop --socket "$SOCKET" >/dev/null 2>&1 || true; fi; rm -rf "$WORKDIR"' EXIT
skip_if_local_socket_bind_unavailable "$WORKDIR/probe.sock"

cd "$PROJECT"
"$GRAFT" init >/dev/null
cat >> .graft/config.toml <<'TOML'
[daemon]
idle_timeout_seconds = 10
TOML
mkdir -p .graft/run/tmp/stale .graft/run/trials/stale .graft/run/worktrees/stale
touch .graft/run/tmp/stale/file .graft/run/trials/stale/file .graft/run/worktrees/stale/file

start_started=$(date +%s)
"$GRAFTD" start --cwd "$PROJECT" --socket "$SOCKET"
start_finished=$(date +%s)
elapsed=$(( start_finished - start_started ))
[[ $elapsed -le 5 ]] || { echo "FAIL: graftd start took ${elapsed}s, expected <=5"; exit 1; }
[[ -S "$SOCKET" ]] || { echo "FAIL: socket missing after start"; exit 1; }
[[ -f "$PROJECT/.graft/run/daemon.pid" ]] || { echo "FAIL: pid file missing after start"; exit 1; }
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
[[ ! -f "$PROJECT/.graft/run/daemon.pid" ]] || { echo "FAIL: pid file lingered after stop"; exit 1; }

python3 -c 'from pathlib import Path; p=Path(".graft/config.toml"); p.write_text(p.read_text().replace("idle_timeout_seconds = 10", "idle_timeout_seconds = 1"))'
"$GRAFTD" start --cwd "$PROJECT" --socket "$SOCKET"
[[ -S "$SOCKET" ]] || { echo "FAIL: restart did not bring up socket"; exit 1; }
for _ in {1..50}; do
  [[ ! -S "$SOCKET" ]] && break
  sleep 0.1
done
[[ ! -S "$SOCKET" ]] || { echo "FAIL: idle timeout did not remove socket"; exit 1; }
[[ ! -f "$PROJECT/.graft/run/daemon.pid" ]] || { echo "FAIL: idle timeout did not remove pid file"; exit 1; }

echo "OK: graftd lifecycle, PID ownership, run-dir cleanup, no .lock, and idle timeout are intact."
