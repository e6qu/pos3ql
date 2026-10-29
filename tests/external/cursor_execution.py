#!/usr/bin/env python3
"""Check when SQL cursors execute, lock rows, and render fetched values."""

# PostgreSQL reference:
# https://www.postgresql.org/docs/18/sql-declare.html
# https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/commands/portalcmds.c

import argparse
import json
from pathlib import Path

import psycopg
from psycopg.types.string import TextLoader

from sql_script import split_sql


def connect(args):
    connection = psycopg.connect(
        host=args.host,
        port=args.port,
        user=args.user,
        dbname=args.database,
        autocommit=True,
        connect_timeout=5,
        prepare_threshold=None,
    )
    connection.execute("SET statement_timeout = '5s'")
    connection.execute("SET lock_timeout = '200ms'")
    connection.execute("SET DateStyle = 'ISO, MDY'")
    connection.adapters.register_loader(1082, TextLoader)
    return connection


def probe(args, trace):
    def rows(connection, label, statement, expected):
        with connection.cursor() as cursor:
            cursor.execute(statement)
            actual = cursor.fetchall()
            entry = {"case": label, "rows": actual, "tag": cursor.statusmessage}
            trace.append(entry)
            assert actual == expected, json.dumps(entry)
        return actual

    def error(connection, label, statement, state):
        try:
            connection.execute(statement)
        except psycopg.Error as failure:
            entry = {
                "case": label,
                "error": failure.sqlstate,
                "message": failure.diag.message_primary,
            }
            trace.append(entry)
            assert failure.sqlstate == state, json.dumps(entry)
        else:
            raise AssertionError(f"{label}: expected SQLSTATE {state}")

    with connect(args) as owner, connect(args) as observer:
        print(f"PostgreSQL server version: {owner.info.server_version}", flush=True)
        assert owner.info.server_version // 10000 == 18
        owner.execute("DROP SCHEMA IF EXISTS cursor_execution_probe CASCADE")
        owner.execute("CREATE SCHEMA cursor_execution_probe")
        owner.execute("SET search_path = cursor_execution_probe")
        observer.execute("SET search_path = cursor_execution_probe")
        try:
            declarations = Path(__file__).with_name("differential_exact") / "35_cursor_declarations.sql"
            declaration_errors = []
            declaration_rows = []
            for line, statement, copy_data in split_sql(declarations.read_text(encoding="utf-8")).statements:
                assert copy_data is None, f"unexpected COPY input at line {line}"
                try:
                    with owner.cursor() as cursor:
                        cursor.execute(statement)
                        if cursor.description is not None:
                            declaration_rows.extend(cursor.fetchall())
                except psycopg.Error as failure:
                    declaration_errors.append((failure.sqlstate, failure.diag.message_primary))
            trace.append({"case": "declarations.errors", "errors": declaration_errors})
            trace.append({"case": "declarations.no_effects", "rows": declaration_rows})
            assert declaration_errors == [
                ("42P11", "cannot specify both SCROLL and NO SCROLL"),
                ("42P11", "cannot specify both SCROLL and NO SCROLL"),
                ("42P11", "cannot specify both ASENSITIVE and INSENSITIVE"),
                ("42P11", "cannot specify both ASENSITIVE and INSENSITIVE"),
                ("0A000", "DECLARE CURSOR WITH HOLD ... FOR UPDATE is not supported"),
                ("0A000", "DECLARE SCROLL CURSOR ... FOR NO KEY UPDATE is not supported"),
                ("42P11", "DECLARE INSENSITIVE CURSOR ... FOR SHARE is not valid"),
                ("0A000", "DECLARE CURSOR WITH HOLD ... FOR KEY SHARE is not supported"),
                ("0A000", "DECLARE CURSOR must not contain data-modifying statements in WITH"),
                ("0A000", "DECLARE CURSOR must not contain data-modifying statements in WITH"),
                ("0A000", "DECLARE CURSOR must not contain data-modifying statements in WITH"),
            ], json.dumps(declaration_errors)
            assert declaration_rows == [(2, 3), (1, False)], json.dumps(declaration_rows)

            owner.execute("CREATE SEQUENCE calls")
            owner.execute("CREATE TABLE source_rows(id integer PRIMARY KEY)")
            owner.execute("INSERT INTO source_rows VALUES (1), (2), (3)")

            owner.execute("BEGIN")
            owner.execute("DECLARE lazy NO SCROLL CURSOR FOR SELECT nextval('calls') FROM generate_series(1,3)")
            rows(owner, "declare.does_not_project", "SELECT last_value,is_called FROM calls", [(1, False)])
            rows(owner, "fetch.zero_before_first", "FETCH 0 FROM lazy", [])
            rows(owner, "fetch.zero_does_not_project", "SELECT last_value,is_called FROM calls", [(1, False)])
            rows(owner, "fetch.first", "FETCH NEXT FROM lazy", [(1,)])
            rows(owner, "fetch.first_issues_one", "SELECT last_value,is_called FROM calls", [(1, True)])
            rows(owner, "fetch.second", "FETCH NEXT FROM lazy", [(2,)])
            owner.execute("CLOSE lazy")
            rows(owner, "close.does_not_finish_query", "SELECT last_value,is_called FROM calls", [(2, True)])
            owner.execute("COMMIT")

            owner.execute("BEGIN")
            owner.execute("DECLARE later_error NO SCROLL CURSOR FOR SELECT 1/(g-2) FROM generate_series(1,3) AS g")
            rows(owner, "error.first_row_succeeds", "FETCH NEXT FROM later_error", [(-1,)])
            error(owner, "error.waits_for_affected_fetch", "FETCH NEXT FROM later_error", "22012")
            owner.execute("ROLLBACK")

            owner.execute("BEGIN")
            owner.execute("DECLARE locked NO SCROLL CURSOR FOR SELECT id FROM source_rows ORDER BY id FOR UPDATE")
            observer.execute("BEGIN")
            rows(observer, "declare.does_not_lock_rows", "SELECT id FROM source_rows WHERE id=1 FOR UPDATE NOWAIT", [(1,)])
            observer.execute("ROLLBACK")
            rows(owner, "locking.first_fetch", "FETCH NEXT FROM locked", [(1,)])
            observer.execute("BEGIN")
            error(observer, "locking.fetched_row", "SELECT id FROM source_rows WHERE id=1 FOR UPDATE NOWAIT", "55P03")
            observer.execute("ROLLBACK")
            observer.execute("BEGIN")
            rows(observer, "locking.unfetched_row", "SELECT id FROM source_rows WHERE id=2 FOR UPDATE NOWAIT", [(2,)])
            observer.execute("ROLLBACK")
            rows(owner, "locking.second_fetch", "FETCH NEXT FROM locked", [(2,)])
            observer.execute("BEGIN")
            error(observer, "locking.second_row", "SELECT id FROM source_rows WHERE id=2 FOR UPDATE NOWAIT", "55P03")
            observer.execute("ROLLBACK")
            owner.execute("CLOSE locked")
            observer.execute("BEGIN")
            error(observer, "close.keeps_transaction_locks", "SELECT id FROM source_rows WHERE id=1 FOR UPDATE NOWAIT", "55P03")
            observer.execute("ROLLBACK")
            owner.execute("COMMIT")
            observer.execute("BEGIN")
            rows(observer, "commit.releases_locks", "SELECT id FROM source_rows WHERE id=1 FOR UPDATE NOWAIT", [(1,)])
            observer.execute("ROLLBACK")

            owner.execute("SELECT setval('calls',100,false)")
            owner.execute("DECLARE outside_block NO SCROLL CURSOR WITH HOLD FOR SELECT nextval('calls') FROM generate_series(1,3)")
            rows(owner, "hold.implicit_commit_finishes_query", "SELECT last_value,is_called FROM calls", [(102, True)])
            rows(owner, "hold.first_saved_row", "FETCH NEXT FROM outside_block", [(100,)])
            owner.execute("CLOSE outside_block")

            owner.execute("SELECT setval('calls',200,false)")
            owner.execute("BEGIN")
            owner.execute("DECLARE partly_read NO SCROLL CURSOR WITH HOLD FOR SELECT nextval('calls') FROM generate_series(1,3)")
            rows(owner, "hold.partial_first_fetch", "FETCH NEXT FROM partly_read", [(200,)])
            rows(owner, "hold.only_fetched_value_before_commit", "SELECT last_value,is_called FROM calls", [(200, True)])
            owner.execute("COMMIT")
            rows(owner, "hold.remaining_values_at_commit", "SELECT last_value,is_called FROM calls", [(202, True)])
            rows(owner, "hold.remaining_saved_rows", "FETCH ALL FROM partly_read", [(201,), (202,)])
            owner.execute("CLOSE partly_read")

            owner.execute("SELECT setval('calls',300,false)")
            owner.execute("BEGIN")
            owner.execute("DECLARE scrolling SCROLL CURSOR FOR SELECT nextval('calls') FROM generate_series(1,3)")
            rows(owner, "scroll.first", "FETCH NEXT FROM scrolling", [(300,)])
            rows(owner, "scroll.second", "FETCH NEXT FROM scrolling", [(301,)])
            rows(owner, "scroll.backward_evaluates_projection", "FETCH PRIOR FROM scrolling", [(302,)])
            rows(owner, "scroll.forward_evaluates_projection", "FETCH NEXT FROM scrolling", [(303,)])
            owner.execute("CLOSE scrolling")
            owner.execute("COMMIT")

            owner.execute("BEGIN")
            owner.execute("DECLARE rendered NO SCROLL CURSOR FOR SELECT DATE '2020-01-02'")
            owner.execute("SET DateStyle = 'SQL, DMY'")
            rows(owner, "render.uses_fetch_settings", "FETCH NEXT FROM rendered", [("02/01/2020",)])
            owner.execute("ROLLBACK")
            owner.execute("DECLARE rendered_hold NO SCROLL CURSOR WITH HOLD FOR SELECT DATE '2020-01-02'")
            owner.execute("SET DateStyle = 'SQL, DMY'")
            rows(owner, "render.saved_values_use_fetch_settings", "FETCH NEXT FROM rendered_hold", [("02/01/2020",)])
            owner.execute("CLOSE rendered_hold")
            owner.execute("SET DateStyle = 'ISO, MDY'")

            owner.execute("SELECT setval('calls',400,false)")
            owner.execute("""CREATE FUNCTION open_native_cursor(p integer) RETURNS refcursor
                LANGUAGE plpgsql AS $$ DECLARE c refcursor := 'native';
                BEGIN OPEN c NO SCROLL FOR SELECT p, nextval('calls') FROM generate_series(1,3);
                RETURN c; END $$""")
            owner.execute("BEGIN")
            rows(owner, "routine.open_returns_cursor", "SELECT open_native_cursor(17)", [("native",)])
            rows(owner, "routine.open_does_not_project", "SELECT last_value,is_called FROM calls", [(400, False)])
            rows(owner, "routine.fetch_keeps_bound_local", "FETCH NEXT FROM native", [(17, 400)])
            owner.execute("CLOSE native")
            rows(owner, "routine.close_does_not_finish_query", "SELECT last_value,is_called FROM calls", [(400, True)])
            owner.execute("COMMIT")
        finally:
            observer.execute("ROLLBACK")
            owner.execute("ROLLBACK")
            owner.execute("CLOSE ALL")
            owner.execute("DROP SCHEMA cursor_execution_probe CASCADE")
    return trace


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--database", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    trace = []
    try:
        probe(args, trace)
    finally:
        args.output.write_text(json.dumps(trace, indent=2) + "\n", encoding="utf-8")
    print(f"cursor execution: {len(trace)} observations passed")


if __name__ == "__main__":
    main()
