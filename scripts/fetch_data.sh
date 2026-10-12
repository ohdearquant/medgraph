#!/usr/bin/env bash
# Download the public inputs into data/raw/ and unpack what the ingest verbs read.
# Every source is a public download; nothing here needs an account or a licence click-through.
# Hashes of the files the maintainers fetched are listed beside each source; a mismatch means the
# publisher replaced the file, so the script prints both values and carries on.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RAW="$ROOT/data/raw"
mkdir -p "$RAW"
UA="medgraph-fetch/0.1"

sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi; }

# name | url | expected sha256 prefix
fetch() {
  local name="$1" url="$2" want="$3" got
  if [ ! -s "$RAW/$name" ]; then
    echo "fetching $name"
    curl --fail --location --silent --show-error -A "$UA" -o "$RAW/$name.part" "$url"
    mv "$RAW/$name.part" "$RAW/$name"
  fi
  got="$(sha256 "$RAW/$name" | cut -c1-16)"
  if [ "$got" = "$want" ]; then
    echo "ok   $name sha256 $got"
  else
    echo "NOTE $name sha256 $got (expected $want): the publisher may have replaced the file" >&2
  fi
}

# CMS FY2026 ICD-10-CM code tables: Tabular List and Alphabetic Index (US federal publication).
fetch cms-code-tables-2026.zip \
  https://www.cms.gov/files/zip/2026-code-tables-tabular-and-index.zip abfc45a54e4edc21
# AHRQ HCUP Clinical Classifications Software Refined, diagnoses, v2026.1.
fetch DXCCSR-v2026-1.zip \
  https://hcup-us.ahrq.gov/toolssoftware/ccsr/DXCCSR-v2026-1.zip 093ea8f925606eea
# CMS risk adjustment, 2026 midyear final ICD-10-CM to HCC mappings.
fetch cms-hcc-2026-midyear.zip \
  https://www.cms.gov/files/zip/2026-midyear-final-icd-10-mappings.zip 1143bc102311fa6f

# Unpack the members the ingest verbs read.
unpack() { # zip, member-glob, output name
  local z="$1" glob="$2" out="$3" member
  [ -s "$RAW/$out" ] && return 0
  member="$(unzip -Z1 "$RAW/$z" | grep -i -- "$glob" | head -n1)"
  [ -n "$member" ] || { echo "no member matching $glob in $z" >&2; exit 1; }
  unzip -p "$RAW/$z" "$member" > "$RAW/$out"
}
unpack cms-code-tables-2026.zip 'icd10cm_tabular_2026\.xml$' cms2026_tabular.xml
unpack cms-code-tables-2026.zip 'icd10cm_index_2026\.xml$' icd10cm_index_2026.xml
unpack DXCCSR-v2026-1.zip 'DXCCSR.*\.csv$' ccsr.csv
unpack cms-hcc-2026-midyear.zip 'icd.*\.csv$' hcc.csv

for f in cms2026_tabular.xml icd10cm_index_2026.xml ccsr.csv hcc.csv; do
  [ -s "$RAW/$f" ] || { echo "missing $RAW/$f" >&2; exit 1; }
done
echo "data ready in $RAW"
