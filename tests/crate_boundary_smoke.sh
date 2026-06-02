#!/usr/bin/env bash
# tests/crate_boundary_smoke.sh
#
# Pins the runtime/frontend boundary: graftd must execute command runtime
# through graft-runtime, not by depending on the graft-cli frontend crate.

set -euo pipefail

cd "$(dirname "$0")/.."

metadata=$(cargo metadata --format-version 1 --no-deps)
python3 -c '
import json
import sys

metadata = json.load(sys.stdin)
packages = {package["name"]: package for package in metadata["packages"]}

daemon_deps = {dep["name"] for dep in packages["graft-daemon"]["dependencies"]}
cli_deps = {dep["name"] for dep in packages["graft-cli"]["dependencies"]}

if "graft-cli" in daemon_deps:
    raise SystemExit("FAIL: graft-daemon must not depend on graft-cli")
if "graft-runtime" not in daemon_deps:
    raise SystemExit("FAIL: graft-daemon should depend on graft-runtime")
if cli_deps != {"anyhow", "graft-runtime"}:
    raise SystemExit(f"FAIL: graft-cli should stay a thin frontend, got deps {sorted(cli_deps)}")
' <<<"$metadata"

echo "OK: graft-daemon depends on graft-runtime, and graft-cli stays a thin frontend."
