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


def index_validation_transcript(reader, writer) -> list[str]:
    writer.execute("DROP TABLE IF EXISTS index_validation_diff")
    writer.execute("CREATE TABLE index_validation_diff (id integer, value integer)")
    writer.execute("INSERT INTO index_validation_diff VALUES (1, 10), (2, 20)")
    results = []
    try:
        for change, expected_old in [
            ("UPDATE index_validation_diff SET value = 10 WHERE id = 2", 20),
            ("INSERT INTO index_validation_diff VALUES (2, 10)", None),
        ]:
            reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
            before = reader.execute("SELECT value FROM index_validation_diff WHERE id = 2").fetchone()
            assert before == (None if expected_old is None else (expected_old,)), before
            writer.execute(change)
            reader.execute("SAVEPOINT before_validation")
            result = sqlstate(reader, "CREATE UNIQUE INDEX index_validation_key ON index_validation_diff(value)")
            reader.execute("ROLLBACK TO SAVEPOINT before_validation")
            after = reader.execute("SELECT value FROM index_validation_diff WHERE id = 2").fetchone()
            results.append(f"index_new_commits={result},snapshot_unchanged={before == after}")
            reader.execute("ROLLBACK")
            writer.execute("DELETE FROM index_validation_diff WHERE id = 2")
        writer.execute("INSERT INTO index_validation_diff VALUES (2, 20)")
        reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        before = reader.execute("SELECT value FROM index_validation_diff WHERE id = 2").fetchone()
        writer.execute("UPDATE index_validation_diff SET value = 30 WHERE id = 2")
        reader.execute("CREATE UNIQUE INDEX index_validation_key ON index_validation_diff(value)")
        after = reader.execute("SELECT value FROM index_validation_diff WHERE id = 2").fetchone()
        results.append(f"index_valid_current=ok,snapshot_unchanged={before == after}")
        reader.execute("ROLLBACK")
        writer.execute("CREATE UNIQUE INDEX index_validation_key ON index_validation_diff(value)")
        results.append(f"index_enforces_current={sqlstate(writer, 'INSERT INTO index_validation_diff VALUES (3, 30)')}")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("DROP TABLE index_validation_diff")


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
        ] + index_validation_transcript(reader, writer)
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
