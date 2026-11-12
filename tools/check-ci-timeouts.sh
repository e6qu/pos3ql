#!/usr/bin/env bash
# Every PR workflow must have the same bounded wall-clock contract. Keep this
# mechanical so a future one-off exception cannot silently return.
set -euo pipefail
cd "$(dirname "$0")/.."

limit=15
failed=0
if ! python3 tools/check-workflow-timeouts.py --limit "$limit" .github/workflows/*.yml; then
    failed=1
fi

# Coverage tracing and crash torture require separate release builds. A matrix
# entry that combines their shards can exceed its bounded job ceiling.
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

# Instrumented library tests include a cold release build and must retain
# enough explicit workers to fit beneath the fixed 15-minute ceiling.
coverage_matrix=.github/workflows/coverage.yml
for coverage_partition in 0-of-6 1-of-6 2-of-6 3-of-6 4-of-6 5-of-6; do
    if ! grep -Fq -- "lib:$coverage_partition" "$coverage_matrix"; then
        printf 'CI timeout guard: missing coverage library partition %s\n' "$coverage_partition" >&2
        failed=1
    fi
done

# The forced-spill suite must distribute corpus work and its independent
# auxiliary probes. Each worker has a fixed 15-minute ceiling.
spill_matrix=.github/workflows/coverage.yml
for spill_entry in \
    '- { name: a, corpus_shard: "0-of-12", auxiliary: none }' \
    '- { name: g, corpus_shard: "6-of-12", auxiliary: none }' \
    '- { name: b, corpus_shard: "1-of-6", auxiliary: none }' \
    '- { name: c, corpus_shard: "2-of-6", auxiliary: none }' \
    '- { name: d, corpus_shard: "3-of-6", auxiliary: none }' \
    '- { name: e, corpus_shard: "4-of-6", auxiliary: none }' \
    '- { name: f, corpus_shard: "5-of-6", auxiliary: none }' \
    '- { name: exact-a, corpus_shard: "none", auxiliary: exact, exact_shard: "0", exact_shards: "3" }' \
    '- { name: exact-b, corpus_shard: "none", auxiliary: exact, exact_shard: "1", exact_shards: "3" }' \
    '- { name: exact-c, corpus_shard: "none", auxiliary: exact, exact_shard: "2", exact_shards: "3" }' \
    '- { name: copy, corpus_shard: "none", auxiliary: copy }' \
    '- { name: types, corpus_shard: "none", auxiliary: types }' \
    '- { name: grouping, corpus_shard: "none", auxiliary: grouping }' \
    '- { name: slt-a, corpus_shard: "none", auxiliary: slt, slt_query_shard: "0", slt_query_shards: "8" }' \
    '- { name: slt-b, corpus_shard: "none", auxiliary: slt, slt_query_shard: "1", slt_query_shards: "8" }' \
    '- { name: slt-c, corpus_shard: "none", auxiliary: slt, slt_query_shard: "2", slt_query_shards: "8" }' \
    '- { name: slt-d, corpus_shard: "none", auxiliary: slt, slt_query_shard: "3", slt_query_shards: "8" }' \
    '- { name: slt-e, corpus_shard: "none", auxiliary: slt, slt_query_shard: "4", slt_query_shards: "8" }' \
    '- { name: slt-f, corpus_shard: "none", auxiliary: slt, slt_query_shard: "5", slt_query_shards: "8" }' \
    '- { name: slt-g, corpus_shard: "none", auxiliary: slt, slt_query_shard: "6", slt_query_shards: "8" }' \
    '- { name: slt-h, corpus_shard: "none", auxiliary: slt, slt_query_shard: "7", slt_query_shards: "8" }'; do
    if ! grep -Fq -- "$spill_entry" "$spill_matrix"; then
        printf 'CI timeout guard: missing forced-spill shard definition %s\n' "$spill_entry" >&2
        failed=1
    fi
done

# Mixed partition widths must cover every ordinal exactly once, including
# future corpora. Check one complete repetition of their assignment cycle.
if ! python3 - "$spill_matrix" <<'PY'
import math
import re
import sys
from pathlib import Path

text = Path(sys.argv[1]).read_text()
for job in ("spill-differential", "reference-diff"):
    match = re.search(rf"(?ms)^  {job}:\n(.*?)(?=^  [a-z][a-z-]*:\n|\Z)", text)
    if match is None:
        sys.exit(f"CI timeout guard: missing {job} matrix")
    partitions = [(int(index), int(count)) for index, count in
                  re.findall(r'corpus_shard: "([0-9]+)-of-([0-9]+)"', match[1])]
    if not partitions or any(count == 0 or index >= count for index, count in partitions):
        sys.exit(f"CI timeout guard: invalid {job} corpus partitions")
    for ordinal in range(math.lcm(*(count for _, count in partitions))):
        owners = sum(ordinal % count == index for index, count in partitions)
        if owners != 1:
            sys.exit(f"CI timeout guard: {job} corpus ordinal {ordinal} has {owners} owners")
PY
then
    failed=1
fi

# Coverage instrumentation plus the complete SQL differential workload no
# longer fits combined workers under the 15-minute ceiling. Keep all corpus
# slices and each independent auxiliary phase explicit.
reference_matrix=.github/workflows/coverage.yml
for reference_entry in \
    '- { name: corpus-a, corpus_shard: "0-of-6", auxiliary: none }' \
    '- { name: corpus-b, corpus_shard: "1-of-12", auxiliary: none }' \
    '- { name: corpus-g, corpus_shard: "7-of-12", auxiliary: none }' \
    '- { name: corpus-c, corpus_shard: "2-of-6", auxiliary: none }' \
    '- { name: corpus-d, corpus_shard: "3-of-6", auxiliary: none }' \
    '- { name: corpus-e, corpus_shard: "4-of-6", auxiliary: none }' \
    '- { name: corpus-f, corpus_shard: "5-of-6", auxiliary: none }' \
    '- { name: auxiliary-exact-a, corpus_shard: "none", auxiliary: exact, exact_shard: "0", exact_shards: "3" }' \
    '- { name: auxiliary-exact-b, corpus_shard: "none", auxiliary: exact, exact_shard: "1", exact_shards: "3" }' \
    '- { name: auxiliary-exact-c, corpus_shard: "none", auxiliary: exact, exact_shard: "2", exact_shards: "3" }' \
    '- { name: auxiliary-copy, corpus_shard: "none", auxiliary: copy }' \
    '- { name: auxiliary-types, corpus_shard: "none", auxiliary: types }' \
    '- { name: auxiliary-grouping, corpus_shard: "none", auxiliary: grouping, grouping_timeout: "300" }' \
    '- { name: auxiliary-pg-regress-a, corpus_shard: "none", auxiliary: pg_regress, pg_regress_shard: "0" }' \
    '- { name: auxiliary-pg-regress-b, corpus_shard: "none", auxiliary: pg_regress, pg_regress_shard: "1" }' \
    '- { name: auxiliary-pg-regress-c, corpus_shard: "none", auxiliary: pg_regress, pg_regress_shard: "2" }' \
    '- { name: auxiliary-pg-regress-d, corpus_shard: "none", auxiliary: pg_regress, pg_regress_shard: "3" }' \
    '- { name: auxiliary-slt-a, corpus_shard: "none", auxiliary: slt, slt_query_shard: "0", slt_query_shards: "8" }' \
    '- { name: auxiliary-slt-b, corpus_shard: "none", auxiliary: slt, slt_query_shard: "1", slt_query_shards: "8" }' \
    '- { name: auxiliary-slt-c, corpus_shard: "none", auxiliary: slt, slt_query_shard: "2", slt_query_shards: "8" }' \
    '- { name: auxiliary-slt-d, corpus_shard: "none", auxiliary: slt, slt_query_shard: "3", slt_query_shards: "8" }' \
    '- { name: auxiliary-slt-e, corpus_shard: "none", auxiliary: slt, slt_query_shard: "4", slt_query_shards: "8" }' \
    '- { name: auxiliary-slt-f, corpus_shard: "none", auxiliary: slt, slt_query_shard: "5", slt_query_shards: "8" }' \
    '- { name: auxiliary-slt-g, corpus_shard: "none", auxiliary: slt, slt_query_shard: "6", slt_query_shards: "8" }' \
    '- { name: auxiliary-slt-h, corpus_shard: "none", auxiliary: slt, slt_query_shard: "7", slt_query_shards: "8" }'; do
    if ! grep -Fq -- "$reference_entry" "$reference_matrix"; then
        printf 'CI timeout guard: missing reference differential shard definition %s\n' "$reference_entry" >&2
        failed=1
    fi
done

if (( $(grep -Fc -- "POS3QL_EXACT_SHARDS: \${{ matrix.exact_shards || '1' }}" "$reference_matrix") != 2 )); then
    printf '%s\n' 'CI timeout guard: instrumented exact-error corpora must pass their file slices' >&2
    failed=1
fi

if ! grep -Fq -- 'POS3QL_POSTGRES_REGRESS_SHARDS: "4"' "$reference_matrix"; then
    printf '%s\n' 'CI timeout guard: instrumented PostgreSQL regression inputs must retain four file slices' >&2
    failed=1
fi

# PostgreSQL-width unit fixtures no longer fit behind build and lint in one
# worker. Keep the complete library suite split across explicit partitions.
ci_workflow=.github/workflows/ci.yml
for test_partition in 0-of-12 1-of-12 2-of-12 3-of-12 4-of-12 5-of-12 \
    6-of-12 7-of-12 8-of-12 9-of-12 10-of-12 11-of-12; do
    if ! grep -Fq -- "$test_partition" "$ci_workflow"; then
        printf 'CI timeout guard: missing library test partition %s\n' "$test_partition" >&2
        failed=1
    fi
done

# The differential matrix owns every disjoint deterministic phase and all ten
# slices of the original seeded fuzz sequence.
differential_workflow=.github/workflows/differential.yml
if grep -nE '^[[:space:]]*cargo[[:space:]]+(build|run|test)' tests/external/ci_diff.sh; then
    printf '%s\n' 'CI timeout guard: comparison shards must use the verified release artifact without rebuilding' >&2
    failed=1
fi
if ! grep -Fq -- '    needs: build' "$differential_workflow"; then
    printf '%s\n' 'CI timeout guard: comparison shards require the shared release build' >&2
    failed=1
fi
for differential_shard in \
    slt-1 slt-2 slt-3 slt-4 slt-5 slt-6 slt-7 slt-8 \
    fuzz-1 fuzz-2 fuzz-3 fuzz-4 fuzz-5 \
    fuzz-6 fuzz-7 fuzz-8 fuzz-9 fuzz-10 core \
    corpus-1 corpus-2 corpus-3 corpus-4 corpus-execution-widths \
    auxiliary-pg-regress-a auxiliary-pg-regress-b \
    auxiliary-pg-regress-c auxiliary-pg-regress-d auxiliary-pg-regress-e \
    auxiliary-exact auxiliary-copy auxiliary-types \
    auxiliary-listen auxiliary-composites; do
    if ! grep -Fq -- "- shard: $differential_shard" "$differential_workflow"; then
        printf 'CI timeout guard: missing differential shard %s\n' "$differential_shard" >&2
        failed=1
    fi
done
if (( $(grep -Fc 'pg_regress_shard: "0"' "$differential_workflow") != 1 )) \
    || (( $(grep -Fc 'pg_regress_shard: "1"' "$differential_workflow") != 1 )) \
    || (( $(grep -Fc 'pg_regress_shard: "2"' "$differential_workflow") != 1 )) \
    || (( $(grep -Fc 'pg_regress_shard: "3"' "$differential_workflow") != 1 )) \
    || (( $(grep -Fc 'pg_regress_shard: "7"' "$differential_workflow") != 1 )) \
    || (( $(grep -Fc 'pg_regress_shards: "8"' "$differential_workflow") != 2 )) \
    || ! grep -Fq -- "POSTGRES_REGRESS_SHARDS: \${{ matrix.pg_regress_shards || '4' }}" "$differential_workflow"; then
    printf '%s\n' 'CI timeout guard: PostgreSQL regression inputs must retain complete file slices' >&2
    failed=1
fi
# Split file slices retain upstream dependency order and exactly one owner.
if ! python3 - "$differential_workflow" <<'PY_GUARD'
import math
import re
import sys
from pathlib import Path

text = Path(sys.argv[1]).read_text()
blocks = re.split(r"(?m)^          - shard: ", text)[1:]
partitions = []
for block in blocks:
    index = re.search(r'pg_regress_shard: "([0-9]+)"', block)
    if index is None:
        continue
    count = re.search(r'pg_regress_shards: "([0-9]+)"', block)
    partitions.append((int(index[1]), int(count[1]) if count else 4))
if not partitions or any(count == 0 or index >= count for index, count in partitions):
    sys.exit("CI timeout guard: invalid PostgreSQL regression file partitions")
for ordinal in range(math.lcm(*(count for _, count in partitions))):
    owners = sum(ordinal % count == index for index, count in partitions)
    if owners != 1:
        sys.exit(f"CI timeout guard: PostgreSQL regression file ordinal {ordinal} has {owners} owners")
PY_GUARD
then
    failed=1
fi

for auxiliary_phase in pg_regress exact copy types listen composites; do
    if ! grep -Fq -- "auxiliary_phase: $auxiliary_phase" "$differential_workflow"; then
        printf 'CI timeout guard: missing auxiliary phase %s\n' "$auxiliary_phase" >&2
        failed=1
    fi
done
if ! grep -Fq -- 'SLT_QUERY_SHARDS: "8"' "$differential_workflow"; then
    printf '%s\n' 'CI timeout guard: sqllogictest queries must retain eight slices' >&2
    failed=1
fi
if (( $(grep -Fc 'corpus_exclude: "179_remaining_execution_widths"' "$differential_workflow") != 4 )) \
    || ! grep -Fq -- 'corpus_only: "179_remaining_execution_widths"' "$differential_workflow"; then
    printf '%s\n' 'CI timeout guard: execution-width corpus must have one isolated worker' >&2
    failed=1
fi
if (( $(grep -Fc 'fuzz_count: "1000"' "$differential_workflow") != 10 )); then
    printf '%s\n' 'CI timeout guard: differential fuzz must retain ten 1,000-statement slices' >&2
    failed=1
fi
for fuzz_start in 0 1000 2000 3000 4000 5000 6000 7000 8000 9000; do
    if ! grep -Fq -- "fuzz_start: \"$fuzz_start\"" "$differential_workflow"; then
        printf 'CI timeout guard: missing differential fuzz start %s\n' "$fuzz_start" >&2
        failed=1
    fi
done

# Four independent VOPR ranges preserve the complete 16-seed corpus while
# keeping each range, including a cold rebuild, within its five-minute cap.
vopr_workflow=.github/workflows/ci.yml
for vopr_range in \
    '- { first: 460259, last: 460262 }' \
    '- { first: 460263, last: 460266 }' \
    '- { first: 460267, last: 460270 }' \
    '- { first: 460271, last: 460274 }'; do
    if ! grep -Fq -- "$vopr_range" "$vopr_workflow"; then
        printf 'CI timeout guard: missing storage VOPR range %s\n' "$vopr_range" >&2
        failed=1
    fi
done
vopr_invocations=$(grep -Fc 'POS3QL_STORAGE_VOPR_SEED0=${{ matrix.first }} cargo test' "$vopr_workflow")
if (( vopr_invocations != 1 )); then
    printf 'CI timeout guard: storage VOPR must run one range per job (found %s invocations)\n' "$vopr_invocations" >&2
    failed=1
fi

(( failed == 0 )) || exit 1
printf 'CI timeout guard: every declared timeout is at most %s minutes\n' "$limit"
