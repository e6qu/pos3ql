#!/usr/bin/env python3
"""Select disjoint ownership groups and verify every selected proof ran once."""

import argparse
from pathlib import Path
import re


GROUPS = {"memory-sql": ("mem::", "sql::"), "storage": ("storage::",)}


def read_manifest(path):
    names = Path(path).read_text().splitlines()
    if not names or len(names) != len(set(names)) or any(
        re.fullmatch(r"[A-Za-z0-9_]+(?:::[A-Za-z0-9_]+)+", name) is None
        for name in names
    ):
        raise ValueError("ownership manifest must contain unique, complete test names")
    for name in names:
        owners = [group for group, prefixes in GROUPS.items() if name.startswith(prefixes)]
        if len(owners) != 1:
            raise ValueError(f"ownership proof must belong to exactly one group: {name}")
    return names


def select(names, group):
    selected = [name for name in names if name.startswith(GROUPS[group])]
    if not selected:
        raise ValueError(f"ownership group is empty: {group}")
    return selected


def verify(log, expected):
    passed = re.findall(r"^test (\S+)(?: - should panic)? \.\.\. ok$", log, re.MULTILINE)
    results = re.findall(
        r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", log, re.MULTILINE
    )
    if len(results) != 1 or results[0] != (str(len(passed)), "0", "0"):
        raise ValueError("Miri did not report one complete passing test run")
    missing = sorted(set(expected) - set(passed))
    unexpected = sorted(set(passed) - set(expected))
    if missing or unexpected or len(passed) != len(expected):
        raise ValueError(f"Miri ownership proof mismatch: missing={missing}, unexpected={unexpected}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("select", "verify"))
    parser.add_argument("manifest", type=Path)
    parser.add_argument("group", choices=tuple(GROUPS))
    parser.add_argument("--log", type=Path)
    args = parser.parse_args()
    if args.action == "verify" and args.log is None:
        parser.error("verify requires --log")
    if args.action == "select" and args.log is not None:
        parser.error("select does not accept --log")
    try:
        expected = select(read_manifest(args.manifest), args.group)
        if args.action == "select":
            print("\n".join(expected))
        else:
            verify(args.log.read_text(), expected)
            print(f"Miri ownership [{args.group}]: {len(expected)} named proofs passed, none failed or ignored")
    except ValueError as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
