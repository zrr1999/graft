# shellcheck shell=bash

_SMOKE_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=tests/lib/socket_probe.sh
source "$_SMOKE_LIB_DIR/socket_probe.sh"

setup_bins() {
  cargo build -p graft-cli

  GRAFT_BIN="$PWD/target/debug/graft"
  GRAFTD_BIN="$PWD/target/debug/graftd"
  GRAFT="$GRAFT_BIN"
  GRAFTD="$GRAFTD_BIN"
  export GRAFT_BIN GRAFTD_BIN GRAFT GRAFTD
  export GRAFT_DAEMON_BIN="$GRAFTD_BIN"
}

setup_workspace() {
  WORKDIR="$(mktemp -d)"
  GRAFT_HOME="$WORKDIR/graft-home"
  SOCKET="$GRAFT_HOME/run/daemon.sock"
  export WORKDIR GRAFT_HOME SOCKET
}

cleanup_daemon() {
  local socket_path="${1:-}"
  if [[ -z "$socket_path" && -n "${SOCKET:-}" ]]; then
    socket_path="$SOCKET"
  fi
  if [[ -z "$socket_path" && -n "${GRAFT_HOME:-}" ]]; then
    socket_path="$GRAFT_HOME/run/daemon.sock"
  fi

  local daemon_bin="${GRAFTD_BIN:-${GRAFTD:-}}"
  if [[ -n "$daemon_bin" && -n "$socket_path" && -S "$socket_path" ]]; then
    "$daemon_bin" stop --socket "$socket_path" >/dev/null 2>&1 || true
  fi
}

cleanup_workspace() {
  cleanup_daemon
  if [[ -n "${WORKDIR:-}" ]]; then
    rm -rf "$WORKDIR"
  fi
}

require_local_socket_bind() {
  local probe_path="${1:-$WORKDIR/probe.sock}"
  require_local_socket_bind_available "$probe_path"
}

write_properties_roto() {
  cat > properties.roto
}

lock_properties() {
  local graft_bin="${GRAFT_BIN:-${GRAFT:-}}"
  "$graft_bin" property lock >/dev/null
}

extract_materialize_path() {
  sed -nE 's/^.*(would write state into| into) (.*)$/\2/p' | tail -n1
}
