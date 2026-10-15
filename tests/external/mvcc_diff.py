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
            before = reader.execute(
                "SELECT value FROM index_validation_diff WHERE id = 2"
            ).fetchone()
            assert before == (None if expected_old is None else (expected_old,)), before
            writer.execute(change)
            reader.execute("SAVEPOINT before_validation")
            result = sqlstate(
                reader,
                "CREATE UNIQUE INDEX index_validation_key ON index_validation_diff(value)",
            )
            reader.execute("ROLLBACK TO SAVEPOINT before_validation")
            after = reader.execute(
                "SELECT value FROM index_validation_diff WHERE id = 2"
            ).fetchone()
            results.append(
                f"index_new_commits={result},snapshot_unchanged={before == after}"
            )
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
        enforced = sqlstate(writer, "INSERT INTO index_validation_diff VALUES (3, 30)")
        results.append(f"index_enforces_current={enforced}")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("DROP TABLE index_validation_diff")


def analyze_snapshot_transcript(reader, writer) -> list[str]:
    writer.execute("DROP TABLE IF EXISTS analyze_snapshot_diff CASCADE")
    writer.execute("CREATE TABLE analyze_snapshot_diff (id integer, a integer, b integer, label text)")
    writer.execute("INSERT INTO analyze_snapshot_diff VALUES (1, 1, 1, 'old'), (2, 1, 1, 'old')")
    writer.execute("CREATE STATISTICS analyze_snapshot_ab (ndistinct, dependencies, mcv) ON a, b FROM analyze_snapshot_diff")
    writer.execute("ANALYZE analyze_snapshot_diff")
    results = []

    def report(stage):
        count = reader.execute("SELECT count(*) FROM analyze_snapshot_diff").fetchone()[0]
        estimated = reader.execute("SELECT reltuples::integer FROM pg_class WHERE oid = 'analyze_snapshot_diff'::regclass").fetchone()[0]
        nulls = reader.execute("SELECT null_frac > 0.3 AND null_frac < 0.4 FROM pg_stats WHERE tablename = 'analyze_snapshot_diff' AND attname = 'label'").fetchone()[0]
        frequent = reader.execute("SELECT most_common_freqs[1] > 0.6 AND most_common_freqs[1] < 0.7 FROM pg_stats_ext WHERE statistics_name = 'analyze_snapshot_ab'").fetchone()[0]
        results.append(f"analyze_{stage}=rows:{estimated},query:{count},nulls:{nulls},mcv:{frequent}")

    try:
        reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        assert reader.execute("SELECT count(*) FROM analyze_snapshot_diff").fetchone()[0] == 2
        writer.execute("INSERT INTO analyze_snapshot_diff VALUES (3, 2, 2, NULL)")
        reader.execute("ANALYZE analyze_snapshot_diff")
        report("committed")
        writer.execute("BEGIN")
        writer.execute("UPDATE analyze_snapshot_diff SET a = 9, label = NULL WHERE id = 1")
        writer.execute("DELETE FROM analyze_snapshot_diff WHERE id = 2")
        writer.execute("INSERT INTO analyze_snapshot_diff VALUES (4, 9, 9, NULL)")
        reader.execute("ANALYZE analyze_snapshot_diff")
        report("foreign_pending")
        writer.execute("ROLLBACK")
        reader.execute("SAVEPOINT before_changes")
        reader.execute("UPDATE analyze_snapshot_diff SET a = 4, b = 4, label = NULL WHERE id = 1")
        reader.execute("DELETE FROM analyze_snapshot_diff WHERE id = 2")
        reader.execute("INSERT INTO analyze_snapshot_diff VALUES (4, 4, 4, NULL)")
        reader.execute("ANALYZE analyze_snapshot_diff")
        own_nulls = reader.execute("SELECT null_frac = 1 FROM pg_stats WHERE tablename = 'analyze_snapshot_diff' AND attname = 'label'").fetchone()[0]
        own_count = reader.execute("SELECT count(*) FROM analyze_snapshot_diff").fetchone()[0]
        results.append(f"analyze_own=all_null:{own_nulls},query:{own_count}")
        reader.execute("ROLLBACK TO SAVEPOINT before_changes")
        report("rolled_back")
        reader.execute("CREATE STATISTICS analyze_snapshot_error ON (100 / (id - 3)) FROM analyze_snapshot_diff")
        reader.execute("SAVEPOINT before_error")
        error = sqlstate(reader, "ANALYZE analyze_snapshot_diff")
        reader.execute("ROLLBACK TO SAVEPOINT before_error")
        unchanged = reader.execute("SELECT count(*) FROM analyze_snapshot_diff").fetchone()[0]
        results.append(f"analyze_error={error},query:{unchanged}")
        reader.execute("DROP STATISTICS analyze_snapshot_error")
        reader.execute("ANALYZE analyze_snapshot_diff")
        report("retried")
        reader.execute("ROLLBACK")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("ROLLBACK")
        writer.execute("DROP TABLE analyze_snapshot_diff CASCADE")


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
        ] + index_validation_transcript(reader, writer) + analyze_snapshot_transcript(reader, writer)
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
