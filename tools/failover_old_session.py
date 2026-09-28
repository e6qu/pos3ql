#!/usr/bin/env python3
"""Hold a primary session across promotion and verify its next write is fenced."""

import argparse
import pathlib
import time

from benchmark import PgConnection


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--ready-file", required=True)
    parser.add_argument("--continue-file", required=True)
    parser.add_argument("--result-file", required=True)
    args = parser.parse_args()

    connection = PgConnection("127.0.0.1", args.port, "postgres", "postgres", 30)
    try:
        rows = connection.query(
            "CREATE TABLE failover_value (value integer); "
            "INSERT INTO failover_value VALUES (41); "
            "SELECT value FROM failover_value"
        )
        if rows != [["41"]]:
            raise RuntimeError(f"unexpected primary rows: {rows!r}")
        pathlib.Path(args.ready_file).write_text("ready\n", encoding="ascii")

        deadline = time.monotonic() + 60
        while not pathlib.Path(args.continue_file).exists():
            if time.monotonic() >= deadline:
                raise TimeoutError("promotion did not release the primary session")
            time.sleep(0.05)

        try:
            connection.query("INSERT INTO failover_value VALUES (99)")
        except RuntimeError as error:
            if "40001" not in str(error):
                raise
            pathlib.Path(args.result_file).write_text("fenced\n", encoding="ascii")
        else:
            raise RuntimeError("displaced primary accepted a durable write")
    finally:
        connection.close()


if __name__ == "__main__":
    main()
