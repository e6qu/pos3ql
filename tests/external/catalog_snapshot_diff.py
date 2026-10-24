#!/usr/bin/env python3
"""Compare catalog rows and current object lookup against PostgreSQL 18."""

import argparse
import json
from pathlib import Path

import psycopg


def connect(host, port, user, database):
    connection = psycopg.connect(
        host=host,
        port=port,
        user=user,
        dbname=database,
        autocommit=True,
        connect_timeout=5,
        prepare_threshold=None,
    )
    connection.execute("SET statement_timeout = '5s'")
    connection.execute("SET lock_timeout = '200ms'")
    return connection


def transcript(reader, writer):
    observed = []

    def record(label, sql, parameters=()):
        rows = reader.execute(sql, parameters).fetchall()
        observed.append({"step": label, "rows": rows})

    for isolation in ("READ COMMITTED", "REPEATABLE READ"):
        prefix = isolation.lower().replace(" ", "_")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            record(f"{prefix}.create.before", "SELECT count(*) FROM pg_class WHERE relname = 'catalog_new_diff'")
            writer.execute("BEGIN")
            writer.execute("CREATE TABLE catalog_new_diff(id integer PRIMARY KEY, note text)")
            writer.execute("COMMENT ON TABLE catalog_new_diff IS 'created later'")
            writer.execute("INSERT INTO catalog_new_diff VALUES(1, 'later')")
            writer.execute("COMMIT")
            record(f"{prefix}.create.catalog", "SELECT count(*) FROM pg_class WHERE relname = 'catalog_new_diff'")
            record(f"{prefix}.create.lookup", "SELECT to_regclass('catalog_new_diff') IS NOT NULL")
            record(f"{prefix}.create.columns", "SELECT count(*) FROM pg_attribute WHERE attrelid = to_regclass('catalog_new_diff') AND attnum > 0")
            record(f"{prefix}.create.comment", "SELECT obj_description(to_regclass('catalog_new_diff'), 'pg_class')")
            record(f"{prefix}.create.rows", "SELECT count(*) FROM catalog_new_diff")
            reader.execute("COMMIT")
            record(f"{prefix}.create.fresh", "SELECT count(*) FROM catalog_new_diff")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            writer.execute("DROP TABLE IF EXISTS catalog_new_diff")

        writer.execute("CREATE TABLE catalog_old_diff(id integer PRIMARY KEY, note text)")
        writer.execute("COMMENT ON TABLE catalog_old_diff IS 'old table comment'")
        writer.execute("COMMENT ON COLUMN catalog_old_diff.note IS 'old column comment'")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            oid = reader.execute("SELECT oid FROM pg_class WHERE relname = 'catalog_old_diff'").fetchone()[0]
            record(f"{prefix}.rename.before", "SELECT relname FROM pg_class WHERE oid = %s", (oid,))
            writer.execute("BEGIN")
            writer.execute("ALTER TABLE catalog_old_diff RENAME TO catalog_renamed_diff")
            writer.execute("ALTER TABLE catalog_renamed_diff RENAME COLUMN note TO label")
            writer.execute("COMMENT ON TABLE catalog_renamed_diff IS 'new table comment'")
            writer.execute("COMMENT ON COLUMN catalog_renamed_diff.label IS 'new column comment'")
            writer.execute("COMMIT")
            record(f"{prefix}.rename.catalog", "SELECT relname FROM pg_class WHERE oid = %s", (oid,))
            record(f"{prefix}.rename.columns", "SELECT attname FROM pg_attribute WHERE attrelid = %s AND attnum > 0 ORDER BY attnum", (oid,))
            record(f"{prefix}.rename.lookup", "SELECT to_regclass('catalog_old_diff') IS NULL, to_regclass('catalog_renamed_diff')::oid = %s::oid", (oid,))
            record(f"{prefix}.rename.display", "SELECT %s::oid::regclass::text", (oid,))
            record(f"{prefix}.rename.comments", "SELECT obj_description(%s::oid, 'pg_class'), col_description(%s::oid, 2)", (oid, oid))
            record(f"{prefix}.rename.comment_rows", "SELECT d.description FROM pg_description d WHERE d.classoid = 'pg_class'::regclass AND d.objoid = %s ORDER BY d.objsubid", (oid,))
            reader.execute("COMMIT")
            record(f"{prefix}.rename.fresh", "SELECT relname FROM pg_class WHERE oid = %s", (oid,))
        finally:
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            writer.execute("DROP TABLE IF EXISTS catalog_old_diff")
            writer.execute("DROP TABLE IF EXISTS catalog_renamed_diff")

        writer.execute("CREATE TABLE catalog_replace_diff(old_column integer)")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            oid = reader.execute("SELECT oid FROM pg_class WHERE relname = 'catalog_replace_diff'").fetchone()[0]
            writer.execute("BEGIN")
            writer.execute("DROP TABLE catalog_replace_diff")
            writer.execute("CREATE TABLE catalog_replace_diff(new_column text)")
            writer.execute("INSERT INTO catalog_replace_diff VALUES('replacement')")
            writer.execute("COMMIT")
            record(f"{prefix}.replace.catalog", "SELECT oid = %s::oid FROM pg_class WHERE relname = 'catalog_replace_diff'", (oid,))
            record(f"{prefix}.replace.old_columns", "SELECT attname FROM pg_attribute WHERE attrelid = %s AND attnum > 0 ORDER BY attnum", (oid,))
            record(f"{prefix}.replace.lookup", "SELECT to_regclass('catalog_replace_diff')::oid = %s::oid", (oid,))
            record(f"{prefix}.replace.rows", "SELECT count(*) FROM catalog_replace_diff")
            reader.execute("COMMIT")
            record(f"{prefix}.replace.fresh", "SELECT count(*) FROM catalog_replace_diff")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            writer.execute("DROP TABLE IF EXISTS catalog_replace_diff")

        writer.execute("CREATE TYPE catalog_enum_diff AS ENUM('old')")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            record(f"{prefix}.enum.before", "SELECT enumlabel FROM pg_enum WHERE enumtypid = 'catalog_enum_diff'::regtype ORDER BY enumsortorder")
            writer.execute("ALTER TYPE catalog_enum_diff ADD VALUE 'new'")
            record(f"{prefix}.enum.catalog", "SELECT enumlabel FROM pg_enum WHERE enumtypid = 'catalog_enum_diff'::regtype ORDER BY enumsortorder")
            record(f"{prefix}.enum.lookup", "SELECT 'new'::catalog_enum_diff::text")
            reader.execute("COMMIT")
            record(f"{prefix}.enum.fresh", "SELECT enumlabel FROM pg_enum WHERE enumtypid = 'catalog_enum_diff'::regtype ORDER BY enumsortorder")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("DROP TYPE catalog_enum_diff")

        writer.execute("CREATE FUNCTION catalog_routine_diff() RETURNS integer LANGUAGE sql AS 'SELECT 1'")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            record(f"{prefix}.routine.before", "SELECT prosrc FROM pg_proc WHERE proname = 'catalog_routine_diff'")
            writer.execute("CREATE OR REPLACE FUNCTION catalog_routine_diff() RETURNS integer LANGUAGE sql AS 'SELECT 2'")
            record(f"{prefix}.routine.catalog", "SELECT prosrc FROM pg_proc WHERE proname = 'catalog_routine_diff'")
            record(f"{prefix}.routine.call", "SELECT catalog_routine_diff()")
            reader.execute("COMMIT")
            record(f"{prefix}.routine.fresh", "SELECT prosrc FROM pg_proc WHERE proname = 'catalog_routine_diff'")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("DROP FUNCTION catalog_routine_diff()")

        writer.execute("CREATE SEQUENCE catalog_sequence_diff START 1 INCREMENT 1")
        reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
        try:
            record(f"{prefix}.sequence.before", "SELECT seqincrement FROM pg_sequence WHERE seqrelid = 'catalog_sequence_diff'::regclass")
            writer.execute("ALTER SEQUENCE catalog_sequence_diff INCREMENT 5")
            record(f"{prefix}.sequence.catalog", "SELECT seqincrement FROM pg_sequence WHERE seqrelid = 'catalog_sequence_diff'::regclass")
            record(f"{prefix}.sequence.next", "SELECT nextval('catalog_sequence_diff'), nextval('catalog_sequence_diff')")
            reader.execute("COMMIT")
            record(f"{prefix}.sequence.fresh", "SELECT seqincrement FROM pg_sequence WHERE seqrelid = 'catalog_sequence_diff'::regclass")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("DROP SEQUENCE catalog_sequence_diff")
    return observed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--database", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--reference", type=Path)
    arguments = parser.parse_args()
    with connect(arguments.host, arguments.port, arguments.user, arguments.database) as reader, connect(arguments.host, arguments.port, arguments.user, arguments.database) as writer:
        observed = transcript(reader, writer)
    # JSON normalizes driver tuples before comparing two independent servers.
    serialized = json.dumps(observed, indent=2) + "\n"
    arguments.output.write_text(serialized)
    if arguments.reference is not None:
        expected = json.loads(arguments.reference.read_text())
        actual = json.loads(serialized)
        if expected != actual:
            for left, right in zip(expected, actual, strict=True):
                if left != right:
                    print(f"MISMATCH: {left['step']}: PostgreSQL={left['rows']!r}, pos3ql={right['rows']!r}")
            raise SystemExit(1)
    print(f"PASS: {len(observed)} catalog snapshot observations on port {arguments.port}")


if __name__ == "__main__":
    main()
