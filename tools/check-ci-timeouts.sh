#!/usr/bin/env bash
# Every PR workflow must have the same bounded wall-clock contract. Keep this
# mechanical so a future one-off exception cannot silently return.
set -euo pipefail
cd "$(dirname "$0")/.."

limit=15
failed=0
job_count=$(grep -rniE '^[[:space:]]*runs-on:' .github/workflows --include='*.yml' | wc -l | tr -d '[:space:]')
timeout_count=$(grep -rniE '^[[:space:]]*timeout-minutes:' .github/workflows --include='*.yml' | wc -l | tr -d '[:space:]')
if (( job_count != timeout_count )); then
    printf 'CI timeout guard: found %s jobs but %s timeout declarations\n' "$job_count" "$timeout_count" >&2
    failed=1
fi
while IFS=: read -r file_path line text; do
    value=${text##*:}
    value=$(printf '%s' "$value" | tr -d '[:space:]')
    if ! [[ $value =~ ^[0-9]+$ ]] || (( value > limit )); then
        printf '%s:%s: timeout-minutes must be an integer no greater than %s (got %q)\n' "$file_path" "$line" "$limit" "$value" >&2
        failed=1
    fi
done < <(grep -rniE '^[[:space:]]*timeout-minutes:' .github/workflows --include='*.yml')

# Coverage tracing and crash torture require separate release builds. A matrix
# entry that combines their shards can exceed its fixed five-minute ceiling.
if grep -nE 'shards:.*(run:.*runtest:|runtest:.*run:)' .github/workflows/coverage.yml; then
    printf '%s\n' 'CI timeout guard: coverage and runtest shards must use separate matrix entries' >&2
    failed=1
fi

# Shard names become tracefile names. Reject directory syntax at the workflow
# boundary instead of relying on every consumer to sanitize it identically.
if grep -nE 'shards:.*[/\\]' .github/workflows/coverage.yml; then
    printf '%s\n' 'CI timeout guard: coverage shard names must not contain path separators' >&2
    failed=1
fi

# The forced-spill suite must distribute corpus work and its independent
# auxiliary probes. Each worker has a fixed five-minute ceiling.
spill_matrix=.github/workflows/coverage.yml
for spill_entry in \
    '- { name: a, corpus_shard: "0-of-4", auxiliary: none }' \
    '- { name: b, corpus_shard: "1-of-4", auxiliary: none }' \
    '- { name: c, corpus_shard: "2-of-4", auxiliary: none }' \
    '- { name: d, corpus_shard: "3-of-4", auxiliary: none }' \
    '- { name: exact, corpus_shard: "none", auxiliary: exact }' \
    '- { name: copy, corpus_shard: "none", auxiliary: copy }' \
    '- { name: types, corpus_shard: "none", auxiliary: types }' \
    '- { name: slt, corpus_shard: "none", auxiliary: slt }'; do
    if ! grep -Fq -- "$spill_entry" "$spill_matrix"; then
        printf 'CI timeout guard: missing forced-spill shard definition %s\n' "$spill_entry" >&2
        failed=1
    fi
done

(( failed == 0 )) || exit 1
printf 'CI timeout guard: every declared timeout is at most %s minutes\n' "$limit"
