#!/usr/bin/env bash
# tests/promote_config_smoke.sh
#
# Verifies promotion requirements come from explicit policy only:
# [promotion].required_properties or one-shot CLI --require.

set -euo pipefail

cd "$(dirname "$0")/.."

cargo build -p graft-cli -p graft-daemon >/dev/null 2>&1
GRAFT_BIN="$PWD/target/debug/graft"
GRAFTD_BIN="$PWD/target/debug/graftd"

WORKDIR="$(mktemp -d)"
cleanup() {
  find "$WORKDIR" -path '*/.graft/run/daemon.sock' -type s -exec "$GRAFTD_BIN" stop --socket {} \; >/dev/null 2>&1 || true
  rm -rf "$WORKDIR"
}
trap cleanup EXIT
cd "$WORKDIR"

git init >/dev/null
git config user.email smoke@example.invalid
git config user.name "Graft Smoke"
git config commit.gpgsign false
printf 'hello\n' > hello.txt
git add hello.txt
git commit -m base >/dev/null
"$GRAFT_BIN" init >/dev/null

# Remove the generated [promotion] block to exercise fail-loud behavior.
python3 - <<'PY'
from pathlib import Path
p = Path('graft.toml')
s = p.read_text()
start = s.index('\n[promotion]\n')
p.write_text(s[:start] + '\n')
PY

explain=$("$GRAFT_BIN" explain promote)
if ! grep -q 'Promotion require source: missing' <<<"$explain"; then
  echo "FAIL: explain promote did not report missing requirement source"
  echo "$explain"; exit 1
fi
if ! grep -q '\[promotion\]\.required_properties' <<<"$explain"; then
  echo "FAIL: explain promote did not point to explicit promotion policy"
  echo "$explain"; exit 1
fi

printf 'hello changed\n' > hello.txt
create=$("$GRAFT_BIN" create --from graft:empty --expect ValidPatch --message promote-explicit-policy)
candidate=$(grep -oE 'candidate:[0-9a-f]+' <<<"$create" | head -n1)
[[ -n $candidate ]] || { echo "FAIL: no candidate id captured"; echo "$create"; exit 1; }

"$GRAFT_BIN" validate "$candidate" >/dev/null
admit=$("$GRAFT_BIN" admit "$candidate" --require ValidPatch)
patch=$(grep -oE 'patch:[0-9a-f]+' <<<"$admit" | head -n1)
[[ -n $patch ]] || { echo "FAIL: no patch id captured"; echo "$admit"; exit 1; }

if "$GRAFT_BIN" promote "$patch" --to main >promote-missing.out 2>&1; then
  echo "FAIL: promote succeeded without explicit requirements"
  cat promote-missing.out; exit 1
fi
promote_missing=$(cat promote-missing.out)
if ! grep -q 'promotion requires explicit properties' <<<"$promote_missing"; then
  echo "FAIL: promote did not reject missing promotion requirements"
  echo "$promote_missing"; exit 1
fi
if grep -q 'legacy default Builds,TestsPass' <<<"$promote_missing"; then
  echo "FAIL: promote should not mention legacy fallback"
  echo "$promote_missing"; exit 1
fi

cli=$("$GRAFT_BIN" promote "$patch" --to main --require ValidPatch 2>&1)
if ! grep -q 'required evidence: ValidPatch (source: cli)' <<<"$cli"; then
  echo "FAIL: CLI --require should define one-shot promotion requirements"
  echo "$cli"; exit 1
fi

# Re-add an explicit promotion requirement and ensure config source wins.
cat >> graft.toml <<'EOF'

[promotion]
required_properties = ["ValidPatch"]
EOF

configured=$("$GRAFT_BIN" promote "$patch" --to main 2>&1)
if grep -q 'legacy default Builds,TestsPass' <<<"$configured"; then
  echo "FAIL: explicit [promotion] config should not use legacy fallback"
  echo "$configured"; exit 1
fi
if ! grep -q 'required evidence: ValidPatch (source: config)' <<<"$configured"; then
  echo "FAIL: explicit [promotion] config should be source=config"
  echo "$configured"; exit 1
fi

echo "OK: promote requires explicit policy and accepts config or CLI requirements."
