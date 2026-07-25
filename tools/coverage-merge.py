#!/usr/bin/env python3
"""Union lcov tracefiles from the coverage shards and hold the line floor.

Every shard builds the same source at the same commit with the same
toolchain, so the instrumented line sets agree and a plain union is the
whole suite's coverage: a line counts as covered if any shard executed it.
The floor applies only to this merged whole — one shard's percentage means
nothing by itself, which is why tools/coverage.sh refuses to compare a
shard against the floor.

Usage: tools/coverage-merge.py <floor-percent> <tracefile.lcov>...
"""

import sys


def main():
    if len(sys.argv) < 3:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    floor = int(sys.argv[1])
    tracefiles = sys.argv[2:]

    covered_by_line = {}  # (source file, line number) -> bool
    for path in tracefiles:
        source = None
        with open(path) as tracefile:
            for raw in tracefile:
                raw = raw.strip()
                if raw.startswith("SF:"):
                    source = raw[3:]
                elif raw.startswith("DA:"):
                    line_number, count = raw[3:].split(",")[:2]
                    key = (source, int(line_number))
                    covered_by_line[key] = covered_by_line.get(key, False) or int(count) > 0

    if not covered_by_line:
        print("FAIL: the tracefiles contain no line records; nothing was measured")
        return 1

    total = len(covered_by_line)
    covered = sum(covered_by_line.values())
    percent = 100.0 * covered / total
    print(
        f"merged line coverage: {covered}/{total} = {percent:.2f}%"
        f"  (floor {floor}%, {len(tracefiles)} shards)"
    )
    if percent < floor:
        print(f"FAIL: merged line coverage {percent:.2f}% is below the {floor}% floor")
        return 1
    print("OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
