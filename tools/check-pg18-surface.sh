#!/usr/bin/env zsh
set -euo pipefail

inventory=${0:A:h:h}/tests/postgresql18_commands.tsv

typeset -i rows
rows=$(awk -F '\t' '!/^#/ && NF { count++ } END { print count + 0 }' "$inventory")
if (( rows != 183 )); then
  print -u2 -- "FAIL: PostgreSQL 18 command inventory has $rows rows, expected 183"
  exit 1
fi

duplicates=$(awk -F '\t' '!/^#/ && NF { seen[$1]++ } END { for (name in seen) if (seen[name] != 1) print name }' "$inventory")
if [[ -n $duplicates ]]; then
  print -u2 -- "FAIL: duplicate PostgreSQL command inventory rows:"
  print -u2 -- "$duplicates"
  exit 1
fi

invalid=$(awk -F '\t' '!/^#/ && NF && ($2 != "complete" && $2 != "partial" && $2 != "missing") { print NR ":" $0 }' "$inventory")
if [[ -n $invalid ]]; then
  print -u2 -- "FAIL: invalid PostgreSQL command inventory states:"
  print -u2 -- "$invalid"
  exit 1
fi

gaps=$(awk -F '\t' '!/^#/ && NF && $2 != "complete" { print $1 "\t" $2 }' "$inventory")
if [[ -n $gaps ]]; then
  typeset -i count
  count=$(print -r -- "$gaps" | wc -l | tr -d ' ')
  print -u2 -- "FAIL: $count PostgreSQL 18 commands are not complete:"
  print -u2 -- "$gaps"
  exit 1
fi

print -- "PASS: all $rows PostgreSQL 18 commands are complete"
