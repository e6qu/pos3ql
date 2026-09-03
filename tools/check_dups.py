#!/usr/bin/env python3
"""Fail when normalized Rust source clones exceed the checked-in ratchet."""

import argparse
import pathlib
import re
import sys


MIN_LINES = 35
# This local detector counts normalized source lines rather than jscpd token
# lines. Its 0.42% baseline preserves a narrow ratchet for the same committed
# clone set without an external package install.
THRESHOLD_PERCENT = 0.42
LINE_COMMENT = re.compile(r"//.*")


def normalized_lines(path):
    lines = []
    for number, source in enumerate(path.read_text().splitlines(), 1):
        source = LINE_COMMENT.sub("", source)
        source = " ".join(source.split())
        if source:
            lines.append((number, source))
    return lines


def clones(root, minimum):
    seen = {}
    reported = set()
    covered = set()
    matches = []
    total = 0
    for path in sorted((root / "src").rglob("*.rs")):
        lines = normalized_lines(path)
        total += len(lines)
        values = [source for _, source in lines]
        for start in range(len(values) - minimum + 1):
            if (path, start) in covered:
                continue
            window = tuple(values[start : start + minimum])
            prior = seen.setdefault(hash(window), (path, start, window))
            prior_path, prior_start, prior_window = prior
            if prior_window != window or (prior_path == path and prior_start == start):
                continue
            key = (prior_path, prior_start, path, start)
            if key in reported:
                continue
            length = minimum
            while (
                prior_start + length < len(values)
                and start + length < len(values)
                and values[prior_start + length] == values[start + length]
            ):
                length += 1
            reported.add(key)
            for offset in range(length - minimum + 1):
                covered.add((prior_path, prior_start + offset))
                covered.add((path, start + offset))
            matches.append((prior_path, lines[prior_start][0], path, lines[start][0], length))
    return total, matches


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=pathlib.Path, default=pathlib.Path(__file__).resolve().parents[1])
    parser.add_argument("--min-lines", type=int, default=MIN_LINES)
    parser.add_argument("--threshold", type=float, default=THRESHOLD_PERCENT)
    args = parser.parse_args()
    if args.min_lines < 1 or args.threshold < 0:
        parser.error("min-lines must be positive and threshold must be non-negative")
    total, matches = clones(args.root, args.min_lines)
    duplicate_lines = sum(length for *_, length in matches)
    percent = 100 * duplicate_lines / total if total else 0
    if percent > args.threshold:
        for first, first_line, second, second_line, length in matches:
            print(f"clone: {first.relative_to(args.root)}:{first_line} = {second.relative_to(args.root)}:{second_line} ({length} lines)")
    print(f"duplicate code: {duplicate_lines}/{total} normalized lines ({percent:.3f}%; limit {args.threshold:.3f}%)")
    return int(percent > args.threshold)


if __name__ == "__main__":
    sys.exit(main())
