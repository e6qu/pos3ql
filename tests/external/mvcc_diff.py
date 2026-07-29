#!/usr/bin/env python3
"""Two-session MVCC and table-lock differential against PostgreSQL."""

import os
import sys

import psycopg


def connect(port: int):
    return psycopg.connect(
        host=os.environ.get("PGHOST", "127.0.0.1"),
        port=port,
        user=os.environ.get("PGUSER", "postgres"),
        dbname="postgres",
        autocommit=True,
    )


def sqlstate(connection, statement: str) -> str:
    try:
        connection.execute(statement)
    except psycopg.Error as error:
        return error.sqlstate or "none"
    return "ok"


def exercise(port: int) -> list[str]:
    reader = connect(port)
    writer = connect(port)
    try:
        writer.execute("DROP TABLE IF EXISTS mvcc_diff")
        writer.execute("CREATE TABLE mvcc_diff (id integer PRIMARY KEY, value text)")
        writer.execute("INSERT INTO mvcc_diff VALUES (1, 'old')")

        reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        first = reader.execute(
            "SELECT value FROM mvcc_diff WHERE id = 1"
        ).fetchone()[0]
        writer.execute("UPDATE mvcc_diff SET value = 'new' WHERE id = 1")
        second = reader.execute(
            "SELECT value FROM mvcc_diff WHERE id = 1"
        ).fetchone()[0]
        read_only_error = sqlstate(
            reader, "INSERT INTO mvcc_diff VALUES (2, 'forbidden')"
        )
        reader.execute("ROLLBACK")
        after = reader.execute(
            "SELECT value FROM mvcc_diff WHERE id = 1"
        ).fetchone()[0]

        reader.execute("BEGIN")
        reader.execute("LOCK TABLE mvcc_diff IN ACCESS SHARE MODE")
        writer.execute("SET lock_timeout = '50ms'")
        lock_error = sqlstate(
            writer, "ALTER TABLE mvcc_diff ADD COLUMN blocked integer"
        )
        reader.execute("ROLLBACK")
        writer.execute("ALTER TABLE mvcc_diff ADD COLUMN allowed integer")

        return [
            f"snapshot={first},{second},{after}",
            f"read_only={read_only_error}",
            f"lock={lock_error}",
            "ddl_after_unlock=ok",
        ]
    finally:
        reader.close()
        writer.close()


def main() -> int:
    postgres_port = int(os.environ.get("PGPORT", "5432"))
    pos3ql_port = int(os.environ.get("P3_PORT", "15599"))
    expected = exercise(postgres_port)
    observed = exercise(pos3ql_port)
    if observed != expected:
        print("MVCC divergence", file=sys.stderr)
        print(f"PostgreSQL: {expected}", file=sys.stderr)
        print(f"pos3ql:     {observed}", file=sys.stderr)
        return 1
    for line in observed:
        print(line)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
