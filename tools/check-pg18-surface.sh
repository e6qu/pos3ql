#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "$0")/.." && pwd)
inventory=$root/tests/postgresql18_commands.tsv

rows=$(awk -F '\t' '!/^#/ && NF { count++ } END { print count + 0 }' "$inventory")
if (( rows != 183 )); then
  printf 'FAIL: PostgreSQL 18 command inventory has %s rows, expected 183\n' "$rows" >&2
  exit 1
fi

duplicates=$(awk -F '\t' '!/^#/ && NF { seen[$1]++ } END { for (name in seen) if (seen[name] != 1) print name }' "$inventory")
if [[ -n $duplicates ]]; then
  printf '%s\n%s\n' 'FAIL: duplicate PostgreSQL command inventory rows:' "$duplicates" >&2
  exit 1
fi

invalid=$(awk -F '\t' '!/^#/ && NF && ($2 != "complete" && $2 != "partial" && $2 != "missing") { print NR ":" $0 }' "$inventory")
if [[ -n $invalid ]]; then
  printf '%s\n%s\n' 'FAIL: invalid PostgreSQL command inventory states:' "$invalid" >&2
  exit 1
fi

gaps=$(awk -F '\t' '!/^#/ && NF && $2 != "complete" { print $1 "\t" $2 }' "$inventory")
if [[ -n $gaps ]]; then
  count=$(printf '%s\n' "$gaps" | wc -l | tr -d ' ')
  printf 'FAIL: %s PostgreSQL 18 commands are not complete:\n%s\n' "$count" "$gaps" >&2
  exit 1
fi

printf 'PASS: all %s PostgreSQL 18 commands are complete\n' "$rows"
