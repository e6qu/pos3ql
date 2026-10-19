#!/bin/sh
# Run one deterministic slice of the Rust library tests. Test names are stable
# inputs, so ordinal partitioning preserves the complete suite across workers.
set -eu
cd "$(dirname "$0")/.."

partition=${1:-}
index=${partition%%-of-*}
count=${partition#*-of-}
case "$index:$count" in
*[!0-9:]* | :* | *: | *:*:*)
    echo "FAIL: expected INDEX-of-COUNT, got '$partition'" >&2
    exit 1
    ;;
esac
if [ "$count" -lt 2 ] || [ "$index" -ge "$count" ]; then
    echo "FAIL: invalid partition '$partition'" >&2
    exit 1
fi

list=$(mktemp)
trap 'rm -f "$list"' EXIT
cargo test --locked --lib -- --list > "$list"

# Validate cold-read effects before the rest of worker zero. Execute this
# case once overall even if future test names move its ordinal to another worker.
priority_test=sql::tests::atomic_block_reads_function_and_sequence_effects_survive_cold_http_reads_once
grep -Fx "$priority_test: test" "$list" >/dev/null
if [ "$index" -eq 0 ]; then
    python3 tools/test_command.py cargo test --locked --lib -- --exact "$priority_test"
fi

set -- --exact \
    --skip "$priority_test" \
    --skip sim::storage::storage_vopr \
    --skip sql::tests::remaining_execution_widths_are_allocation_free_and_survive_cold_recovery \
    --skip sql::tests::external_cold_order_and_distinct_runs_use_object_storage \
    --skip sql::tests::external_recursive_runs_use_object_storage \
    --skip sql::tests::external_lateral_runs_use_object_storage \
    --skip sql::tests::external_set_runs_use_object_storage
ordinal=0
selected=0
while IFS= read -r line; do
    case "$line" in
    *': test')
        test_name=${line%: test}
        if [ $((ordinal % count)) -eq "$index" ]; then
            selected=$((selected + 1))
        else
            set -- "$@" --skip "$test_name"
        fi
        ordinal=$((ordinal + 1))
        ;;
    esac
done < "$list"
if [ "$selected" -eq 0 ] || [ "$ordinal" -eq "$selected" ]; then
    echo "FAIL: partition $partition selected $selected of $ordinal tests" >&2
    exit 1
fi
echo "library test partition $partition: $selected of $ordinal tests"
python3 tools/test_command.py cargo test --locked --lib -- "$@"
