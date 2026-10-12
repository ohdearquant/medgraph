#!/usr/bin/env bash
# One command: build the binary, fetch the public data, load the ICD-10-CM graph into a local
# SQLite store, then answer three coding questions from the graph. No model key, no account.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo build --release --locked -p icd10-khive
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/icd10-khive"

bash scripts/fetch_data.sh

STORE="$ROOT/data/store"
CFG="$STORE/medgraph.toml"
mkdir -p "$STORE"
# kkernel refuses to serve from a directory that group or other can write. mkdir -p follows the
# caller's umask (0002 on a stock Ubuntu account gives 0775), so set the mode explicitly.
chmod 700 "$STORE"
# The same check walks every directory above the store, and a clone made under umask 0002 leaves
# the checkout root and data/ group-writable too. Both are inside this checkout, so fix them here.
# If a directory above the checkout is the one at fault, the hint printed on failure names it.
chmod g-w,o-w "$ROOT" "$ROOT/data" || true
if [ ! -f "$CFG" ]; then
  cat > "$CFG" <<TOML
[actor]
id = "medgraph"
display_name = "medgraph"

[[backends]]
name = "main"
kind = "sqlite"
path = "$STORE/medgraph.db"
TOML
fi

# KHIVE_NO_DAEMON keeps every call in this process: a khive daemon already running on the machine
# (another version, another store) is never contacted.
# kkernel will not serve from a store whose path has a group- or world-writable directory above it
# (a sticky directory such as /tmp is allowed). When it refuses, list the directories that qualify.
writable_ancestors_hint() {
  local d perm found=""
  d="$(cd "$ROOT" && pwd -P)"
  while [ "$d" != "/" ]; do
    perm="$(ls -ld "$d" | cut -c1-10)"
    if { [ "${perm:5:1}" = w ] || [ "${perm:8:1}" = w ]; } && [ "${perm:9:1}" != t ] && [ "${perm:9:1}" != T ]; then
      found="$found $d"
    fi
    d="$(dirname "$d")"
  done
  if [ -n "$found" ]; then
    echo "kkernel refuses a store with a group- or world-writable directory above it. Writable here:$found" >&2
    echo "Clone into a directory that is not group- or world-writable (for example one with mode 755), or run chmod g-w on each directory listed above." >&2
  fi
}
kk() {
  local rc
  KHIVE_NO_DAEMON=1 KHIVE_PACKS=kg,icd10 KHIVE_REQUEST_READ_TIMEOUT_SECS=3600 "$BIN" exec --config "$CFG" --strict "$@" || { rc=$?; writable_ancestors_hint; return "$rc"; }
}
pp() { if command -v jq >/dev/null 2>&1; then jq .; else cat; fi; }

if [ ! -f "$STORE/.loaded" ]; then
  echo "loading the ICD-10-CM graph"
  kk "icd10.ingest(tabular_path=\"$ROOT/data/raw/cms2026_tabular.xml\", index_path=\"$ROOT/data/raw/icd10cm_index_2026.xml\")"
  kk "icd10.ingest_external(rules_jsonl=\"$ROOT/data/rules/guideline-rules-FY2026.jsonl\", ccsr_csv=\"$ROOT/data/raw/ccsr.csv\", hcc_csv=\"$ROOT/data/raw/hcc.csv\")"
  touch "$STORE/.loaded"
fi

echo "resolve: free text to candidate codes"
kk 'icd10.resolve(mentions=[{"text": "type 2 diabetes with diabetic chronic kidney disease"}])' | pp
echo "neighborhood: what surrounds E11.22"
kk 'icd10.neighborhood(code="E11.22")' | pp
echo "check: is this code set coherent"
kk 'icd10.check(codes=["E11.22", "N18.3"])' | pp
