# shellcheck shell=bash

require_local_socket_bind_available() {
  local socket_path="$1"
  local probe_out
  local probe_status

  set +e
  probe_out=$(python3 - "$socket_path" <<'PY' 2>&1
import errno
import os
import socket
import sys

path = sys.argv[1]
unsupported = {errno.EACCES, errno.EPERM}
for name in ("ENOTSUP", "EOPNOTSUPP"):
    value = getattr(errno, name, None)
    if value is not None:
        unsupported.add(value)

sock = None
try:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.bind(path)
    sock.listen(1)
except OSError as err:
    if err.errno in unsupported:
        print(f"ERROR: local socket bind not permitted in this environment: {err}")
        sys.exit(1)
    raise
finally:
    if sock is not None:
        sock.close()
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass
PY
)
  probe_status=$?
  set -e

  if [[ $probe_status -ne 0 ]]; then
    echo "$probe_out"
    exit "$probe_status"
  fi
}
