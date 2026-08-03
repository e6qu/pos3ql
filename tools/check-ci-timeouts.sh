#!/usr/bin/env zsh
# Every PR workflow must have the same bounded wall-clock contract. Keep this
# mechanical so a future one-off exception cannot silently return.
set -euo pipefail
cd "$(dirname "$0")/.."

limit=15
failed=0
job_count=$(grep -rniE '^[[:space:]]*runs-on:' .github/workflows --include='*.yml' | wc -l | tr -d '[:space:]')
timeout_count=$(grep -rniE '^[[:space:]]*timeout-minutes:' .github/workflows --include='*.yml' | wc -l | tr -d '[:space:]')
if (( job_count != timeout_count )); then
    print -u2 -- "CI timeout guard: found $job_count jobs but $timeout_count timeout declarations"
    failed=1
fi
while IFS=: read -r path line text; do
    value=${text##*:}
    value=${value//[[:space:]]/}
    if [[ $value != <-> || $value -gt $limit ]]; then
        print -u2 -- "$path:$line: timeout-minutes must be an integer no greater than $limit (got '$value')"
        failed=1
    fi
done < <(grep -rniE '^[[:space:]]*timeout-minutes:' .github/workflows --include='*.yml')

(( failed == 0 )) || exit 1
print -- "CI timeout guard: every declared timeout is at most $limit minutes"
