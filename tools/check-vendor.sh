#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"

(cd vendor && shasum -a 256 -c SHA256SUMS)

manifest=tests/external/postgres_regress_schedule.tsv
commit=724edf9bde9d356724ad384a2e196edc3c9f80f7
grep -Fq "$commit" vendor/README.md

while IFS=$'\t' read -r path first_line last_line; do
    case "$path" in
        ""|'#'*) continue ;;
        vendor/test/postgres-regress/sql/*.sql) ;;
        *) echo "invalid PostgreSQL regression path: $path" >&2; exit 1 ;;
    esac
    test -f "$path"
    case "$first_line" in
        ''|*[!0-9]*) echo "invalid first line for $path: $first_line" >&2; exit 1 ;;
    esac
    case "$last_line" in
        ''|*[!0-9]*) echo "invalid last line for $path: $last_line" >&2; exit 1 ;;
    esac
    lines=$(wc -l < "$path")
    if [ "$first_line" -lt 1 ] || { [ "$last_line" -ne 0 ] && [ "$first_line" -gt "$last_line" ]; }; then
        echo "invalid line range for $path: $first_line..$last_line" >&2
        exit 1
    fi
    if [ "$last_line" -ne 0 ] && [ "$last_line" -gt "$lines" ]; then
        echo "line limit exceeds $path: $last_line > $lines" >&2
        exit 1
    fi
    base=${path##*/}
    test -f "vendor/test/postgres-regress/expected/${base%.sql}.out"
done < "$manifest"

awk -F '\t' '
    $0 !~ /^#/ && NF {
        end = ($3 == 0 ? 2147483647 : $3)
        if (($1 in prior_end) && $2 <= prior_end[$1]) {
            printf "overlapping or unordered range for %s at %s..%s\n", $1, $2, $3 > "/dev/stderr"
            exit 1
        }
        prior_end[$1] = end
    }
' "$manifest"

for path in vendor/test/postgres-regress/sql/*.sql; do
    count=$(awk -F '\t' -v path="$path" '$1 == path { count++ } END { print count + 0 }' "$manifest")
    if [ "$count" -lt 1 ]; then
        echo "vendored regression input is absent from manifest: $path" >&2
        exit 1
    fi
done

echo "PASS: vendored corpus integrity and executable manifest"
