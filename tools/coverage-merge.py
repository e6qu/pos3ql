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


def parse_data_record(raw, path, line_number):
    fields = raw[3:].split(",", 2)
    if len(fields) < 2 or not fields[0] or not fields[1]:
        raise ValueError(f"{path}:{line_number}: malformed LCOV data record: {raw!r}")
    try:
        source_line = int(fields[0])
        count = int(fields[1])
    except ValueError as error:
        raise ValueError(
            f"{path}:{line_number}: malformed LCOV data record: {raw!r}"
        ) from error
    if source_line <= 0 or count < 0:
        raise ValueError(f"{path}:{line_number}: malformed LCOV data record: {raw!r}")
    return source_line, count


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
            for trace_line, raw in enumerate(tracefile, start=1):
                raw = raw.strip()
                if raw.startswith("SF:"):
                    source = raw[3:]
                elif raw.startswith("DA:"):
                    try:
                        source_line, count = parse_data_record(raw, path, trace_line)
                    except ValueError as error:
                        print(f"FAIL: {error}")
                        return 1
                    if not source:
                        print(f"FAIL: {path}:{trace_line}: LCOV data record has no source file")
                        return 1
                    key = (source, source_line)
                    covered_by_line[key] = covered_by_line.get(key, False) or count > 0

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
