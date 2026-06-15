#!/usr/bin/env bash
# tests/quality_gates_smoke.sh
#
# Lightweight static gate for convergence invariants that tend to drift in docs
# and smoke tests: public lifecycle commands, bare constraints, application
# integrity wording, and meaningful regression tests.

set -euo pipefail

cd "$(dirname "$0")/.."

python3 <<'PY'
import re
import sys
from pathlib import Path
from typing import Optional

root = Path.cwd()
failures: list[tuple[str, list[str]]] = []


def files_under(paths: list[str], *, suffixes: Optional[tuple[str, ...]] = None, exclude: Optional[set[str]] = None):
    exclude = exclude or set()
    for raw in paths:
        path = root / raw
        if path.is_file():
            candidates = [path]
        else:
            candidates = sorted(p for p in path.rglob("*") if p.is_file())
        for candidate in candidates:
            rel = candidate.relative_to(root).as_posix()
            if rel in exclude or candidate.name in exclude:
                continue
            if suffixes and candidate.suffix not in suffixes:
                continue
            yield candidate


def scan(label: str, pattern: str, paths: list[str], *, suffixes: Optional[tuple[str, ...]] = None, exclude: Optional[set[str]] = None):
    regex = re.compile(pattern)
    matches: list[str] = []
    for path in files_under(paths, suffixes=suffixes, exclude=exclude):
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        rel = path.relative_to(root).as_posix()
        for number, line in enumerate(text.splitlines(), start=1):
            if regex.search(line):
                matches.append(f"{rel}:{number}: {line}")
    if matches:
        failures.append((label, matches))


scan(
    "public docs/templates/explain leaked retired lifecycle or scoped-constraint syntax",
    r"base \+ change == target|workspace:<|<scope>:|workspace = \[|graft materialize|graft promote",
    ["README.md", "templates/default", "crates/graft-explain/src"],
    suffixes=(".md", ".toml", ".roto", ".rs"),
)

scan(
    "smoke tests use hidden top-level lifecycle aliases outside cli_help compatibility checks",
    r"\$GRAFT(_BIN)?\" (candidate from-scratch|validate|admit|show|search|materialize|promote|candidates|incoming|diff)\b|\$GRAFT(_BIN)?\" --json (validate|admit|show|search|materialize|promote)\b",
    ["tests"],
    suffixes=(".sh",),
    exclude={"cli_help_smoke.sh"},
)

scan(
    "tests contain ignored, TODO, unimplemented, or no-op assertions",
    r"#\s*\[ignore\]|todo!\(|unimplemented!\(|assert!\(true\)",
    [
        "tests",
        "crates/graft-core/src",
        "crates/graft-store/src",
        "crates/graft-sync/src",
        "crates/graft-runtime/src",
        "crates/graft-explain/src",
    ],
    suffixes=(".sh", ".rs"),
)

if failures:
    for label, matches in failures:
        print(f"FAIL: {label}")
        for match in matches:
            print(match)
    sys.exit(1)

print("OK: quality gates reject stale public terms, hidden lifecycle main paths, and no-op tests.")
PY
