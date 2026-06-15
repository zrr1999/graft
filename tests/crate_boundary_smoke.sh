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


def require_line_budget(path, max_lines, reason):
    with open(path, encoding="utf-8") as handle:
        lines = sum(1 for _ in handle)
    if lines > max_lines:
        raise SystemExit(
            f"FAIL: {path} has {lines} lines; expected <= {max_lines}: {reason}"
        )


def forbid_source_text(path, forbidden, reason):
    with open(path, encoding="utf-8") as handle:
        source = handle.read()
    if forbidden in source:
        raise SystemExit(f"FAIL: {path} exposes {forbidden!r}: {reason}")


def require_file(path, reason):
    with open(path, encoding="utf-8"):
        pass


def deps(package):
    return {dep["name"] for dep in packages[package]["dependencies"]}


def internal_deps(package):
    return deps(package) & workspace_crates


def require_absent(package, reason):
    if package in packages:
        raise SystemExit(f"FAIL: {package} must not exist: {reason}")


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


# The old graft-git facade is retired: git integration is explicit read/write boundaries.
require_absent(
    "graft-git",
    "repo reads live in graft-repo, external git writes live in graft-promote",
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

# Storage, sync, and git tiers remain leaf implementation crates over graft-core.
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
    "graft-repo",
    {"graft-core"},
    "repo should own git read/base/snapshot behavior without depending on promote/runtime layers",
)
require_exact_internal_deps(
    "graft-promote",
    {"graft-core"},
    "promote should own external git writes without depending on repo/runtime layers",
)
require_exact_internal_deps(
    "graft-policy",
    {"graft-core"},
    "policy should stay a structural decision crate; user-facing diagnostics live in explain/runtime",
)
require_exact_internal_deps(
    "graft-client",
    set(),
    "client should stay a protocol helper without depending on implementation crates",
)

# Large implementation crates must keep their crate roots as routing surfaces,
# not as catch-all implementation files. These budgets intentionally reflect the
# current internal module split, while leaving room for small future additions.
require_line_budget(
    "crates/graft-store/src/lib.rs",
    900,
    "store object/evidence/index/record/virtual-tree internals should stay out of lib.rs",
)
require_line_budget(
    "crates/graft-sync/src/lib.rs",
    300,
    "sync progress/manifest/public-store internals should stay out of lib.rs",
)
require_line_budget(
    "crates/graft-scratch/src/lib.rs",
    200,
    "scratch engine/ops/hashline internals should stay out of lib.rs",
)

for path, reason in {
    "crates/graft-store/src/objects.rs": "store typed object methods live outside lib.rs",
    "crates/graft-store/src/evidence.rs": "store evidence/index refs live outside lib.rs",
    "crates/graft-store/src/index.rs": "store sqlite indexing lives outside lib.rs",
    "crates/graft-store/src/records.rs": "store candidate/patch relation records live outside lib.rs",
    "crates/graft-store/src/virtual_tree.rs": "store virtual tree/materialization lives outside lib.rs",
    "crates/graft-sync/src/manifest.rs": "sync manifest/ref/digest logic lives outside lib.rs",
    "crates/graft-sync/src/progress.rs": "sync divergence/last_synced logic lives outside lib.rs",
    "crates/graft-sync/src/public_store.rs": "sync public object validation/copy logic lives outside lib.rs",
    "crates/graft-scratch/src/engine.rs": "scratch engine operations live outside lib.rs",
    "crates/graft-scratch/src/ops.rs": "scratch tree/path operation helpers live outside lib.rs",
    "crates/graft-scratch/src/hashlines.rs": "scratch hashline mechanism lives outside lib.rs",
}.items():
    require_file(path, reason)

# Internal data/mechanism modules should not become part of the public crate API.
forbid_source_text(
    "crates/graft-sync/src/lib.rs",
    "pub use manifest::{ManifestRecord, ManifestSummary}",
    "sync manifest records are internal wire/store implementation details",
)
forbid_source_text(
    "crates/graft-scratch/src/lib.rs",
    "pub fn render_hashlines",
    "hashline rendering is a scratch-engine mechanism, not crate API",
)
forbid_source_text(
    "crates/graft-scratch/src/lib.rs",
    "pub fn line_hash",
    "hashline anchors are a scratch-engine mechanism, not crate API",
)
'

echo "OK: crate dependency boundaries and crate-root surface budgets are pinned for CLI, runtime, daemon, store, sync, scratch, policy, and client."
