#!/usr/bin/env bash
# tests/tree_cli_smoke.sh
#
# Verifies the read-only graft tree CLI/API for base refs and live scratch refs.

set -euo pipefail

cd "$(dirname "$0")/.."

source tests/lib/smoke.sh

setup_bins
setup_workspace
PROJECT="$WORKDIR/project"
trap cleanup_workspace EXIT
mkdir -p "$PROJECT"
require_local_socket_bind

"$GRAFT" --cwd "$PROJECT" init >/dev/null
seed=$("$GRAFT" --cwd "$PROJECT" scratch write --base graft:empty src/base.ts --content $'export const needle = "base";\n')
seed_scratch=$(last_graft_id scratch "$seed")
[[ -n $seed_scratch ]] || { echo "FAIL: no seed scratch"; echo "$seed"; exit 1; }
candidate_out=$("$GRAFT" --cwd "$PROJECT" patch from-scratch "$seed_scratch" --message tree-smoke-base)
candidate=$(first_graft_id candidate "$candidate_out")
[[ -n $candidate ]] || { echo "FAIL: no candidate"; echo "$candidate_out"; exit 1; }

base_list=$("$GRAFT" --cwd "$PROJECT" --json tree list --base "$candidate" --glob '*.ts' --limit 10)
JSON_PAYLOAD="$base_list" python3 - <<'PY'
import json, os
payload = json.loads(os.environ["JSON_PAYLOAD"])
result = payload["result"]
paths = [entry["path"] for entry in result["entries"]]
assert result["source"]["kind"] == "base", result
assert result["operation"] == "list", result
assert paths == ["src/base.ts"], paths
assert result["total_matches"] == 1, result
assert result["truncated"] is False, result
PY

base_grep=$("$GRAFT" --cwd "$PROJECT" --json tree grep --base "$candidate" needle --glob '*.ts' --limit 1)
JSON_PAYLOAD="$base_grep" python3 - <<'PY'
import json, os
result = json.loads(os.environ["JSON_PAYLOAD"])["result"]
assert result["matches"][0]["path"] == "src/base.ts", result
assert result["matches"][0]["line"] == 1, result
assert result["searched_paths"] == 1, result
assert result["skipped_binary_paths"] == [], result
PY

base_meta=$("$GRAFT" --cwd "$PROJECT" --json tree metadata --base "$candidate" src/base.ts)
JSON_PAYLOAD="$base_meta" python3 - <<'PY'
import json, os
result = json.loads(os.environ["JSON_PAYLOAD"])["result"]
assert result["kind"] == "file", result
assert result["path"] == "src/base.ts", result
assert result["is_utf8_text"] is True, result
assert result["size"] > 0, result
assert "hash" in result and result["hash"], result
PY

changed=$("$GRAFT" --cwd "$PROJECT" scratch write --base "$candidate" src/changed.ts --content $'needle changed\n')
changed_scratch=$(last_graft_id scratch "$changed")
[[ -n $changed_scratch ]] || { echo "FAIL: no changed scratch"; echo "$changed"; exit 1; }

scratch_list=$("$GRAFT" --cwd "$PROJECT" --json tree list --from "$changed_scratch" --path src --glob '*.ts' --limit 10)
JSON_PAYLOAD="$scratch_list" python3 - <<'PY'
import json, os
result = json.loads(os.environ["JSON_PAYLOAD"])["result"]
paths = [entry["path"] for entry in result["entries"]]
assert result["source"]["kind"] == "scratch", result
assert sorted(paths) == ["src/base.ts", "src/changed.ts"], paths
assert result["total_matches"] == 2, result
PY

scratch_grep=$("$GRAFT" --cwd "$PROJECT" --json tree grep --from "$changed_scratch" needle --glob '*.ts' --limit 10)
JSON_PAYLOAD="$scratch_grep" python3 - <<'PY'
import json, os
result = json.loads(os.environ["JSON_PAYLOAD"])["result"]
paths = [match["path"] for match in result["matches"]]
assert sorted(paths) == ["src/base.ts", "src/changed.ts"], paths
assert result["searched_paths"] == 2, result
PY

scratch_meta=$("$GRAFT" --cwd "$PROJECT" --json tree read-metadata --from "$changed_scratch" src)
JSON_PAYLOAD="$scratch_meta" python3 - <<'PY'
import json, os
result = json.loads(os.environ["JSON_PAYLOAD"])["result"]
assert result["operation"] == "metadata", result
assert result["kind"] == "directory", result
assert result["path"] == "src", result
assert result["child_count"] == 2, result
assert result["hash"] is None, result
PY

echo "OK: tree CLI base and scratch inspection works."
