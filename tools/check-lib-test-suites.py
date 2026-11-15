#!/usr/bin/env python3
"""Check that the two library test suites partition the complete test set.

Each argument is a libtest `--list` output: the complete set (built without a
suite feature), the `sql-fixture-suite` executable and the `unit-suite`
executable. Every test must belong to exactly one suite, and the suites split
at the `sql::tests` module.
"""

import sys
from pathlib import Path

FIXTURES = "sql::tests::"


def tests(path):
    names = []
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        if line.endswith(": test"):
            names.append(line[: -len(": test")])
    if not names:
        raise SystemExit(f"library test suite guard: {path} lists no tests")
    if len(names) != len(set(names)):
        raise SystemExit(f"library test suite guard: {path} lists a test twice")
    return set(names)


def main():
    if len(sys.argv) != 4:
        raise SystemExit("usage: check-lib-test-suites.py COMPLETE FIXTURES UNITS")
    complete, fixtures, units = (tests(path) for path in sys.argv[1:])
    failures = []
    for name in sorted(fixtures & units):
        failures.append(f"in both suites: {name}")
    for name in sorted(complete - fixtures - units):
        failures.append(f"in neither suite: {name}")
    for name in sorted((fixtures | units) - complete):
        failures.append(f"missing from the complete set: {name}")
    for name in sorted(fixtures):
        if not name.startswith(FIXTURES):
            failures.append(f"fixture suite runs a unit test: {name}")
    for name in sorted(units):
        if name.startswith(FIXTURES):
            failures.append(f"unit suite runs a SQL fixture: {name}")
    if failures:
        print("\n".join(f"library test suite guard: {failure}" for failure in failures),
              file=sys.stderr)
        raise SystemExit(1)
    print(f"library test suites: {len(fixtures)} fixtures and {len(units)} units "
          f"partition all {len(complete)} tests")


if __name__ == "__main__":
    main()
