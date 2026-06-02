#!/usr/bin/env bash
# tests/explain_smoke.sh
#
# Verifies `graft explain <id>` across the three supported namespaces:
# clap-derived concepts, diagnostic catalog codes, and builtin property
# metadata. Also checks structured --json output and unknown-id suggestions.

set -euo pipefail

cd "$(dirname "$0")/.."

cargo build -p graft-cli -p graft-daemon >/dev/null 2>&1
GRAFT_BIN="$PWD/target/debug/graft"

concept=$("$GRAFT_BIN" explain admit)
if ! grep -qE '^concept: admit$' <<<"$concept"; then
  echo "FAIL: explain admit did not render a concept card"
  echo "$concept"; exit 1
fi
if ! grep -q 'Admit a candidate into the registry' <<<"$concept"; then
  echo "FAIL: explain admit did not reuse clap about text"
  echo "$concept"; exit 1
fi

diag=$("$GRAFT_BIN" explain V003)
if ! grep -qE '^diagnostic: V003$' <<<"$diag"; then
  echo "FAIL: explain V003 did not render diagnostic card"
  echo "$diag"; exit 1
fi
if ! grep -qE '^  fix: ' <<<"$diag" || ! grep -qE '^  see also: ' <<<"$diag"; then
  echo "FAIL: explain V003 missing fix/see-also rows"
  echo "$diag"; exit 1
fi

builtin=$("$GRAFT_BIN" explain ValidPatch)
if ! grep -qE '^builtin property: valid_patch$' <<<"$builtin"; then
  echo "FAIL: explain ValidPatch did not render builtin metadata"
  echo "$builtin"; exit 1
fi
if ! grep -qE '^  failure mode: ' <<<"$builtin"; then
  echo "FAIL: explain ValidPatch missing failure modes"
  echo "$builtin"; exit 1
fi

json=$("$GRAFT_BIN" --json explain V003)
if ! grep -q '"kind": "diagnostic"' <<<"$json"; then
  echo "FAIL: --json explain V003 missing diagnostic kind"
  echo "$json"; exit 1
fi
if ! grep -q '"code": "V003"' <<<"$json"; then
  echo "FAIL: --json explain V003 missing code field"
  echo "$json"; exit 1
fi

set +e
unknown=$("$GRAFT_BIN" explain admt 2>&1)
status=$?
set -e
if [[ $status -eq 0 ]]; then
  echo "FAIL: unknown explain id should exit non-zero"
  echo "$unknown"; exit 1
fi
if ! grep -qE '^unknown explain id: admt$' <<<"$unknown" || ! grep -q 'did you mean: admit' <<<"$unknown"; then
  echo "FAIL: unknown explain id did not show did-you-mean"
  echo "$unknown"; exit 1
fi

echo "OK: explain covers concepts, diagnostic codes, builtin properties, JSON, and suggestions."
