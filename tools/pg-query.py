#!/usr/bin/env python3
"""Run one simple-protocol query without requiring psql or a Python package."""

import argparse
import json
import pathlib
import time

from benchmark import PgConnection


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--user", default="postgres")
    parser.add_argument("--database", default="postgres")
    parser.add_argument("--expect")
    parser.add_argument("--timeout", type=float, default=0)
    parser.add_argument("--output-json")
    parser.add_argument("sql")
    args = parser.parse_args()
    deadline = time.monotonic() + args.timeout
    started = time.monotonic()
    while True:
        try:
            connection = PgConnection(args.host, args.port, args.user, args.database)
            try:
                rows = connection.query(args.sql)
            finally:
                connection.close()
        except (ConnectionError, OSError, TimeoutError):
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)
            continue
        value = rows[0][0] if rows and rows[0] else None
        if args.expect is None or value == args.expect:
            if args.output_json:
                pathlib.Path(args.output_json).write_text(
                    json.dumps(
                        {
                            "schema_version": 1,
                            "expected": args.expect,
                            "observed": value,
                            "elapsed_seconds": time.monotonic() - started,
                        },
                        indent=2,
                        sort_keys=True,
                    )
                    + "\n",
                    encoding="utf-8",
                )
            if value is not None:
                print(value)
            return
        if time.monotonic() >= deadline:
            raise SystemExit(f"expected {args.expect!r}, observed {value!r}")
        time.sleep(0.05)


if __name__ == "__main__":
    main()
