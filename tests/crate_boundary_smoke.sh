#!/usr/bin/env bash
# tests/crate_boundary_smoke.sh
#
# Pins the crate dependency boundaries created by the command-router split.
# This smoke test fails when a crate starts depending on an implementation tier
# it should only reach through the explicit runtime/daemon/store boundary.

set -euo pipefail

cd "$(dirname "$0")/.."

cargo metadata --format-version 1 --no-deps | python3 -c '
import json
import sys

metadata = json.load(sys.stdin)
packages = {package["name"]: package for package in metadata["packages"]}
workspace_crates = {name for name in packages if name.startswith("graft-")}


def deps(package):
    return {dep["name"] for dep in packages[package]["dependencies"]}


def internal_deps(package):
    return deps(package) & workspace_crates


def require_deps(package, required):
    missing = sorted(required - deps(package))
    if missing:
        raise SystemExit(
            f"FAIL: {package} is missing required deps {missing}; got {sorted(deps(package))}"
        )


def forbid_deps(package, forbidden, reason):
    present = sorted(forbidden & deps(package))
    if present:
        raise SystemExit(
            f"FAIL: {package} must not depend on {present}: {reason}; got {sorted(deps(package))}"
        )


def require_exact_deps(package, expected, reason):
    actual = deps(package)
    if actual != expected:
        raise SystemExit(
            f"FAIL: {package} deps should be exactly {sorted(expected)} ({reason}); got {sorted(actual)}"
        )


def require_exact_internal_deps(package, expected, reason):
    actual = internal_deps(package)
    if actual != expected:
        raise SystemExit(
            f"FAIL: {package} internal deps should be exactly {sorted(expected)} ({reason}); got {sorted(actual)}"
        )


# CLI owns the installable user-facing binaries (`graft` and `graftd`) and delegates implementation.
require_exact_deps(
    "graft-cli",
    {"anyhow", "graft-daemon", "graft-runtime"},
    "CLI package should contain only frontend binaries and delegate implementation to runtime/daemon crates",
)

# Daemon owns wire handling and workspace mutation services, but never imports the CLI frontend.
require_deps(
    "graft-daemon",
    {"graft-client", "graft-core", "graft-runtime", "graft-scratch", "graft-store"},
)
forbid_deps(
    "graft-daemon",
    {"graft-cli", "graft-promote", "graft-repo", "graft-sync", "graft-validate"},
    "daemon should route command execution through graft-runtime rather than reimplementing command crates",
)

# Runtime is orchestration and command dispatch, not a frontend or daemon implementation.
require_deps(
    "graft-runtime",
    {"graft-client", "graft-core", "graft-store", "graft-sync", "graft-validate"},
)
forbid_deps(
    "graft-runtime",
    {"graft-cli", "graft-daemon", "graft-scratch"},
    "runtime must stay independent of frontend, daemon process, and daemon-owned scratch engine crates",
)

# Storage and sync tiers remain leaf implementation crates over graft-core.
require_exact_internal_deps(
    "graft-store",
    {"graft-core"},
    "store should be storage-only and not call runtime/daemon/client layers",
)
require_exact_internal_deps(
    "graft-sync",
    {"graft-core"},
    "sync should be transport/store-object logic and not call runtime/daemon/client layers",
)
require_exact_internal_deps(
    "graft-client",
    set(),
    "client should stay a protocol helper without depending on implementation crates",
)
'

echo "OK: crate dependency boundaries are pinned for CLI, runtime, daemon, store, sync, and client."
