#!/usr/bin/env bash
set -euo pipefail

[[ $# == 1 ]] || { echo "usage: test-miri-ownership.sh LOG" >&2; exit 1; }
LOG=$1
TEST_LIST="$(cd "$(dirname "$0")" && pwd)/miri-ownership-tests.txt"
tests=()
while IFS= read -r name; do
  tests+=("$name")
done < "$TEST_LIST"

python3 - "$TEST_LIST" <<'PYVALIDATE'
import pathlib
import re
import sys

expected = pathlib.Path(sys.argv[1]).read_text().splitlines()
if not expected or len(expected) != len(set(expected)) or any(
        re.fullmatch(r"[A-Za-z0-9_]+(?:::[A-Za-z0-9_]+)+", name) is None for name in expected):
    raise SystemExit("Miri ownership manifest must contain unique, complete test names")
PYVALIDATE

# Compile ownership modules and share one interpreter startup for every proof.
cargo miri test --lib --locked --features miri-ownership -- \
  --exact "${tests[@]}" --test-threads=1 --color=never --format=pretty \
  2>&1 | tee "$LOG"
python3 - "$LOG" "$TEST_LIST" <<'PY'
import pathlib
import re
import sys

log = pathlib.Path(sys.argv[1]).read_text()
expected = pathlib.Path(sys.argv[2]).read_text().splitlines()
passed = re.findall(r"^test (\S+)(?: - should panic)? \.\.\. ok$", log, re.MULTILINE)
results = re.findall(r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", log, re.MULTILINE)
if len(results) != 1 or results[0] != (str(len(passed)), "0", "0"):
    raise SystemExit("Miri did not report one complete passing test run")
missing = sorted(set(expected) - set(passed))
unexpected = sorted(set(passed) - set(expected))
if missing or unexpected or len(passed) != len(expected):
    raise SystemExit(f"Miri ownership proof mismatch: missing={missing}, unexpected={unexpected}")
print(f"Miri ownership: {len(passed)} named proofs passed, none failed or ignored")
PY
