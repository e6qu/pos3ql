"""Check timeout ownership in the repository's block-style workflow jobs."""

import argparse
from pathlib import Path
import re
import sys


def errors(text, limit):
    problems = []
    in_jobs = False
    job = None
    runner = False
    timeouts = 0

    def finish_job():
        if runner and timeouts != 1:
            problems.append(f"job {job}: requires exactly one job timeout (got {timeouts})")

    for number, line in enumerate(text.splitlines(), 1):
        if re.match(r"^[^\s#]", line):
            finish_job()
            job, runner, timeouts = None, False, 0
            in_jobs = line == "jobs:"
        if not in_jobs:
            continue
        declaration = re.fullmatch(r"  ([A-Za-z_][A-Za-z_0-9-]*):\s*(?:#.*)?", line)
        if declaration:
            finish_job()
            job, runner, timeouts = declaration[1], False, 0
        if job is None:
            continue
        if re.match(r"    runs-on:", line):
            runner = True
        timeout = re.fullmatch(r"( +)timeout-minutes:\s*([^#]*?)(?:\s*#.*)?", line)
        if timeout is None:
            continue
        value = timeout[2].strip()
        if not re.fullmatch(r"[0-9]+", value) or not 1 <= int(value) <= limit:
            problems.append(f"line {number}: timeout-minutes must be an integer from 1 to {limit}")
        if len(timeout[1]) == 4:
            timeouts += 1
    finish_job()
    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--limit", type=int, required=True)
    parser.add_argument("workflows", type=Path, nargs="+")
    args = parser.parse_args()
    failed = False
    for path in args.workflows:
        for problem in errors(path.read_text(), args.limit):
            print(f"CI timeout guard: {path}: {problem}", file=sys.stderr)
            failed = True
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
