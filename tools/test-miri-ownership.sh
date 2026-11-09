#!/usr/bin/env bash
set -euo pipefail

[[ $# == 2 ]] || { echo "usage: test-miri-ownership.sh LOG GROUP" >&2; exit 1; }
LOG=$1
GROUP=$2
TOOLS="$(cd "$(dirname "$0")" && pwd)"
TEST_LIST="$TOOLS/miri-ownership-tests.txt"

python3 "$TOOLS/miri-ownership-manifest.py" select "$TEST_LIST" "$GROUP" > "$LOG.manifest"
tests=()
while IFS= read -r name; do
  tests+=("$name")
done < "$LOG.manifest"

# Each runner shares one interpreter startup for its complete, disjoint group.
cargo miri test --lib --locked --features miri-ownership -- \
  --exact "${tests[@]}" --test-threads=1 --color=never --format=pretty \
  2>&1 | tee "$LOG"
python3 "$TOOLS/miri-ownership-manifest.py" verify "$TEST_LIST" "$GROUP" --log "$LOG"
