#!/usr/bin/env bash
# Fixture number guard: every exact-error fixture has its own ordinal prefix,
# so a fixture is named unambiguously by its number in logs and plans.
#
# Usage: tools/check-fixture-numbers.sh [DIR]   (exit 0 = unique, 1 = duplicates)

set -u
cd "$(dirname "$0")/.."

dir=${1:-tests/external/differential_exact}
status=0
unnumbered=$(find "$dir" -maxdepth 1 -name '*.sql' ! -name '[0-9]*_*.sql' -print)
if [[ -n "$unnumbered" ]]; then
  printf 'fixture without a number prefix:\n%s\n' "$unnumbered"
  status=1
fi
duplicates=$(find "$dir" -maxdepth 1 -name '[0-9]*_*.sql' -exec basename {} \; \
  | sed -E 's/^0*([0-9]+)_.*/\1/' | sort | uniq -d)
for number in $duplicates; do
  printf 'fixture number %s is used by:\n' "$number"
  find "$dir" -maxdepth 1 -name "$number"'_*.sql' -o -name "0$number"'_*.sql' | sort | sed 's/^/  /'
  status=1
done
if (( status == 0 )); then
  printf 'fixture number guard: %s fixtures, numbers unique\n' \
    "$(find "$dir" -maxdepth 1 -name '*.sql' | wc -l | tr -d ' ')"
fi
exit "$status"
