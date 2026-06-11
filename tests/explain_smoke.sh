#!/usr/bin/env bash
# tests/explain_smoke.sh
#
# Verifies `graft explain <id>` across the three supported namespaces:
# clap-derived concepts, diagnostic catalog codes, and builtin evaluator
# metadata. Also checks structured --json output and unknown-id suggestions.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins

concept=$("$GRAFT_BIN" explain admit)
if ! grep -qE '^concept: admit$' <<<"$concept"; then
  echo "FAIL: explain admit did not render a concept card"
  echo "$concept"; exit 1
fi
if ! grep -q 'Admit a candidate into the registry' <<<"$concept"; then
  echo "FAIL: explain admit did not reuse clap about text"
  echo "$concept"; exit 1
fi

workflow=$("$GRAFT_BIN" explain agent-workflow)
if ! grep -qE '^concept: agent-workflow$' <<<"$workflow"; then
  echo "FAIL: explain agent-workflow did not render the stable workflow topic"
  echo "$workflow"; exit 1
fi
for required in \
  'scratch is daemon-backed draft state' \
  'graft patch from-scratch' \
  'admit generates a public patch' \
  'graft patch materialize <patch-id>' \
  'repo add/sync/lock/update' \
  'bundle import' \
  'workspace gc --apply' \
  'read/inspect commands on the local CLI path' \
  'graft_cli_exec'; do
  if ! grep -Fq "$required" <<<"$workflow"; then
    echo "FAIL: explain agent-workflow missing required guidance: $required"
    echo "$workflow"; exit 1
  fi
done
for retired in 'graft create' 'graft candidate from-scratch' 'graft validate candidate:' 'graft admit candidate:' 'scratch open' 'scratch promote' 'admit --capture'; do
  if grep -Fq "$retired" <<<"$workflow"; then
    echo "FAIL: explain agent-workflow leaked retired main path: $retired"
    echo "$workflow"; exit 1
  fi
done

workflow_alias=$("$GRAFT_BIN" explain workflow)
if ! grep -qE '^concept: workflow$' <<<"$workflow_alias"; then
  echo "FAIL: explain workflow alias did not render a concept card"
  echo "$workflow_alias"; exit 1
fi

explain_help=$("$GRAFT_BIN" explain --help)
if ! grep -q 'agent-workflow' <<<"$explain_help"; then
  echo "FAIL: explain --help missing agent-workflow example"
  echo "$explain_help"; exit 1
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

builtin=$("$GRAFT_BIN" explain changed_paths_any_match)
if ! grep -qE '^builtin evaluator: changed_paths_any_match$' <<<"$builtin"; then
  echo "FAIL: explain changed_paths_any_match did not render builtin evaluator metadata"
  echo "$builtin"; exit 1
fi
if ! grep -qE '^  input: ' <<<"$builtin" || ! grep -qE '^  predicate: ' <<<"$builtin"; then
  echo "FAIL: explain changed_paths_any_match missing atomic input/predicate rows"
  echo "$builtin"; exit 1
fi
if ! grep -qE '^  failure mode: ' <<<"$builtin"; then
  echo "FAIL: explain changed_paths_any_match missing failure modes"
  echo "$builtin"; exit 1
fi

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT
(
  cd "$WORKDIR"
  "$GRAFT_BIN" workspace init >/dev/null
  write_properties_roto <<'ROTO'
fn empty_change(app: Application) -> Property {
    property(
        [
            app.changed_paths().any_match(["**"]).failure(),
        ],
        "the change touches no paths",
        Severity.Blocking,
        [],
    )
}
ROTO
  lock_properties
  property=$("$GRAFT_BIN" explain empty_change)
  if ! grep -qE '^concept: empty_change$' <<<"$property"; then
    echo "FAIL: explain empty_change did not render configured property alias"
    echo "$property"; exit 1
  fi
  if ! grep -q 'static v2 PropertyPlan' <<<"$property"; then
    echo "FAIL: explain empty_change did not describe its v2 property-plan source"
    echo "$property"; exit 1
  fi
)

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

echo "OK: explain covers concepts, agent workflow guidance, diagnostic codes, builtin evaluators, JSON, and suggestions."
