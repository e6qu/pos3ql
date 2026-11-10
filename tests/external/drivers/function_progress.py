#!/usr/bin/env python3
"""Compare once-only function effects across real client waits and cancellation."""

import argparse
import json
import threading
import time

import psycopg


def exercise(host, port, user, protocol, shape):
    factory = psycopg.ClientCursor if protocol == "simple" else psycopg.Cursor
    options = dict(host=host, port=port, user=user, dbname="postgres",
                   autocommit=True, connect_timeout=5, cursor_factory=factory)
    with psycopg.connect(**options) as admin, psycopg.connect(**options) as writer:
        admin.execute("SET statement_timeout='5s'")
        writer.execute("SET statement_timeout='5s'")
        admin.execute("DROP SCHEMA IF EXISTS function_progress_driver CASCADE")
        admin.execute("CREATE SCHEMA function_progress_driver")
        for connection in (admin, writer):
            connection.execute("SET search_path=function_progress_driver,public")
            connection.execute("SET timezone='UTC'")
        admin.execute("CREATE SEQUENCE issued")
        admin.execute("CREATE TABLE orders(id integer,payload text)")
        admin.execute("CREATE TABLE audit(stage text,n bigint,payload text)")
        settings = ""
        sql_body = shape.startswith("sql_")
        if shape == "timezone":
            signature = "RETURNS timestamptz"
            settings = "SET timezone='Europe/Bucharest'"
            finish = "RETURN substr('2026-01-01 00:00:00',1);"
            call = "SELECT waiting(NEW.payload) INTO answer"
        elif shape in ("scalar", "initializer", "domain", "sql_scalar", "sql_atomic", "sql_write", "sql_return_domain"):
            signature = "RETURNS text"
            finish = "RETURN saved||':'||n::text;"
            call = "SELECT waiting(NEW.payload) INTO answer"
        else:
            signature = "RETURNS TABLE(payload text)"
            finish = "payload:=saved; RETURN NEXT; payload:=saved||':'||n::text; RETURN NEXT; RETURN;"
            call = "SELECT string_agg(payload,',') INTO answer FROM waiting(NEW.payload)"
        child = """DECLARE n bigint:=nextval('issued'); saved text:=p;
            BEGIN INSERT INTO audit VALUES('before',n,saved);
            PERFORM pg_advisory_xact_lock(846201);
            INSERT INTO audit VALUES('after',n,saved);"""
        if sql_body:
            if shape in ("sql_return_domain", "sql_table_domain"):
                admin.execute(f"CREATE FUNCTION sql_check(p text) RETURNS boolean LANGUAGE plpgsql STRICT AS $$ {child} RETURN true; END$$")
                admin.execute("CREATE DOMAIN sql_checked_payload AS text CHECK(sql_check(VALUE))")
                sql_signature = "RETURNS TABLE(payload sql_checked_payload)" if shape == "sql_table_domain" else "RETURNS sql_checked_payload"
                final = "SELECT p"
            else:
                admin.execute(f"CREATE FUNCTION sql_child(p text) {signature} LANGUAGE plpgsql AS $$ {child} {finish} END$$")
                sql_signature = signature
                final = "SELECT * FROM sql_child(p)" if shape == "sql_table" else "SELECT sql_child(p)"
                if shape == "sql_write":
                    admin.execute("CREATE TABLE sql_payload(payload text)")
                    admin.execute("""CREATE FUNCTION sql_payload_after() RETURNS trigger LANGUAGE plpgsql AS $$
                        BEGIN PERFORM sql_child(NEW.payload); RETURN NEW; END$$""")
                    admin.execute("CREATE TRIGGER sql_payload_after AFTER INSERT ON sql_payload FOR EACH ROW EXECUTE FUNCTION sql_payload_after()")
                    final = "INSERT INTO sql_payload VALUES(p) RETURNING payload"
            sql = f"INSERT INTO audit VALUES('sql-prelude',0,p); {final}"
            body = f"BEGIN ATOMIC {sql}; END" if shape == "sql_atomic" else f"AS $${sql}$$"
            admin.execute(f"CREATE FUNCTION waiting(p text) {sql_signature} LANGUAGE SQL {body}")
        elif shape == "initializer":
            admin.execute(f"CREATE FUNCTION initialize_payload(p text) RETURNS text LANGUAGE plpgsql AS $$ {child} RETURN saved||':'||n::text; END$$")
            admin.execute("""CREATE FUNCTION waiting(p text) RETURNS text LANGUAGE plpgsql AS $$
                DECLARE saved text:=initialize_payload(p); BEGIN RETURN saved; END$$""")
        elif shape == "domain":
            admin.execute(f"CREATE FUNCTION check_payload(p text) RETURNS boolean LANGUAGE plpgsql STRICT AS $$ {child} RETURN true; END$$")
            admin.execute("CREATE DOMAIN checked_payload AS text CHECK(check_payload(VALUE))")
            admin.execute("""CREATE FUNCTION waiting(p text) RETURNS text LANGUAGE plpgsql AS $$
                DECLARE saved checked_payload:=p; BEGIN RETURN saved||':'||currval('issued')::text; END$$""")
        else:
            admin.execute(f"""CREATE FUNCTION waiting(p text) {signature} LANGUAGE plpgsql {settings} AS $$
                {child} {finish} END$$""")
        admin.execute(f"""CREATE FUNCTION save_order() RETURNS trigger LANGUAGE plpgsql AS $$
            DECLARE answer text; BEGIN {call};
            INSERT INTO audit VALUES('caller',NEW.id,answer); RETURN NEW; END$$""")
        admin.execute("CREATE TRIGGER save_order AFTER INSERT ON orders FOR EACH ROW EXECUTE FUNCTION save_order()")
        pid = writer.execute("SELECT pg_backend_pid()").fetchone()[0]
        transcript = []
        try:
            for cancel, order in [(False, 1), (True, 2)]:
                admin.execute("SELECT pg_advisory_lock(846201)")
                outcome = []
                diagnostics = []

                def insert():
                    try:
                        with writer.cursor() as cursor:
                            cursor.execute("INSERT INTO orders VALUES(%s,repeat(%s,8192)) RETURNING id",
                                           (order, "x"))
                            outcome.append(("rows", cursor.fetchall()))
                    except psycopg.Error as error:
                        outcome.append(("error", error.sqlstate))
                        diagnostics.append(str(error))

                worker = threading.Thread(target=insert, daemon=True)
                worker.start()
                deadline = time.monotonic() + 5
                try:
                    while time.monotonic() < deadline:
                        blockers = admin.execute("SELECT pg_blocking_pids(%s)", (pid,)).fetchone()[0]
                        if blockers:
                            break
                        assert worker.is_alive(), (outcome, diagnostics)
                        time.sleep(0.01)
                    else:
                        raise AssertionError("function never reached its held lock")
                    assert admin.execute("SELECT 40+2").fetchone() == (42,)
                    assert admin.execute("SELECT last_value FROM issued").fetchone() == (order,)
                    assert admin.execute("SELECT count(*) FROM audit").fetchone() == ((order - 1) * (4 if sql_body else 3),)
                    if cancel:
                        assert admin.execute("SELECT pg_cancel_backend(%s)", (pid,)).fetchone() == (True,)
                    else:
                        assert admin.execute("SELECT pg_advisory_unlock(846201)").fetchone() == (True,)
                    worker.join(timeout=5)
                    assert not worker.is_alive(), "function did not finish after release/cancellation"
                    assert outcome == ([("error", "57014")] if cancel else [("rows", [(order,)])]), (outcome, diagnostics)
                    transcript.append(outcome)
                finally:
                    admin.execute("SELECT pg_advisory_unlock_all()")
                    if worker.is_alive():
                        admin.execute("SELECT pg_cancel_backend(%s)", (pid,))
                        worker.join(timeout=5)
            writer.execute("INSERT INTO orders VALUES(%s,%s)", (3, "fresh"))
            expected_length = {"scalar": 8194, "initializer": 8194, "domain": 8194, "table": 16387, "timezone": 22,
                               "sql_scalar": 8194, "sql_atomic": 8194, "sql_table": 16387,
                               "sql_write": 8192, "sql_return_domain": 8192, "sql_table_domain": 8192}[shape]
            expected_fresh = {"scalar": 7, "initializer": 7, "domain": 7, "table": 13, "timezone": 22,
                              "sql_scalar": 7, "sql_atomic": 7, "sql_table": 13,
                              "sql_write": 5, "sql_return_domain": 5, "sql_table_domain": 5}[shape]
            audit = admin.execute("SELECT stage,n,length(payload) FROM audit ORDER BY n,stage,length(payload)").fetchall()
            prefix = [("sql-prelude", 0, 5), ("sql-prelude", 0, 8192)] if sql_body else []
            assert audit == prefix + [("after", 1, 8192), ("before", 1, 8192), ("caller", 1, expected_length),
                             ("after", 3, 5), ("before", 3, 5), ("caller", 3, expected_fresh)], audit
            if shape == "timezone":
                assert admin.execute("SELECT payload FROM audit WHERE stage='caller' ORDER BY n").fetchall() == [
                    ("2025-12-31 22:00:00+00",), ("2025-12-31 22:00:00+00",)]
            assert admin.execute("SELECT id FROM orders ORDER BY id").fetchall() == [(1,), (3,)]
            assert admin.execute("SELECT last_value FROM issued").fetchone() == (3,)
            if shape == "sql_write":
                assert admin.execute("SELECT length(payload) FROM sql_payload ORDER BY length(payload)").fetchall() == [(5,), (8192,)]
            print(json.dumps(dict(protocol=protocol, shape=shape, outcomes=transcript, audit=audit), sort_keys=True))
        finally:
            admin.execute("SELECT pg_advisory_unlock_all()")
            admin.execute("DROP SCHEMA function_progress_driver CASCADE")


def result_order(host, port, user, protocol):
    factory = psycopg.ClientCursor if protocol == "simple" else psycopg.Cursor
    with psycopg.connect(host=host, port=port, user=user, dbname="postgres",
                        autocommit=True, connect_timeout=5, cursor_factory=factory) as connection:
        connection.execute("DROP SCHEMA IF EXISTS sql_result_order_driver CASCADE")
        connection.execute("CREATE SCHEMA sql_result_order_driver")
        connection.execute("SET search_path=sql_result_order_driver,public")
        try:
            connection.execute("CREATE SEQUENCE issued")
            connection.execute("CREATE TABLE audit(n bigint)")
            connection.execute("CREATE TABLE target(n bigint)")
            connection.execute("CREATE DOMAIN positive AS bigint NOT NULL CHECK(VALUE>0)")
            connection.execute("""CREATE FUNCTION first_value() RETURNS bigint LANGUAGE SQL AS $$
                INSERT INTO audit VALUES(0); SELECT nextval('issued') FROM generate_series(1,3) $$""")
            assert connection.execute("SELECT first_value()").fetchone() == (1,)
            assert connection.execute("SELECT last_value FROM issued").fetchone() == (1,)
            connection.execute("""CREATE FUNCTION checked_rows() RETURNS SETOF positive LANGUAGE SQL AS $$
                INSERT INTO audit VALUES(1); SELECT nextval('issued')-3 FROM generate_series(1,3) $$""")
            try:
                connection.execute("SELECT * FROM checked_rows()").fetchall()
                raise AssertionError("invalid first result was accepted")
            except psycopg.Error as error:
                assert error.sqlstate == "23514", str(error)
            assert connection.execute("SELECT last_value FROM issued").fetchone() == (2,)
            assert connection.execute("SELECT n FROM audit ORDER BY n").fetchall() == [(0,)]
            connection.execute("""CREATE FUNCTION all_writes() RETURNS bigint LANGUAGE SQL AS $$
                INSERT INTO target VALUES(nextval('issued')),(nextval('issued')) RETURNING n $$""")
            assert connection.execute("SELECT all_writes()").fetchone() == (3,)
            assert connection.execute("SELECT n FROM target ORDER BY n").fetchall() == [(3,), (4,)]
            connection.execute("""CREATE FUNCTION record_overflow(OUT a integer,OUT b integer) LANGUAGE SQL AS $$
                INSERT INTO audit VALUES(9); SELECT 2147483648::bigint,1 $$""")
            try:
                connection.execute("SELECT record_overflow()").fetchall()
                raise AssertionError("out-of-range declared record field was accepted")
            except psycopg.Error as error:
                assert error.sqlstate == "22003", str(error)
            assert connection.execute("SELECT n FROM audit ORDER BY n").fetchall() == [(0,)]
            connection.execute("""CREATE FUNCTION empty_value() RETURNS positive LANGUAGE SQL AS $$
                SELECT NULL::bigint WHERE false $$""")
            empty = connection.execute("SELECT empty_value() IS NULL").fetchone()[0]
            assert empty is True
            connection.execute("""CREATE FUNCTION lock_receipt() RETURNS integer LANGUAGE plpgsql AS $$
                BEGIN RETURN 42; END$$""")
            connection.execute("""SELECT pg_advisory_lock(301),pg_advisory_lock_shared(302),
                pg_advisory_lock(303,1),pg_advisory_lock_shared(304,1)""")
            unlocked = connection.execute("""SELECT pg_advisory_unlock(301),pg_advisory_unlock_shared(302),
                pg_advisory_unlock(303,1),pg_advisory_unlock_shared(304,1),lock_receipt()""").fetchone()
            assert unlocked == (True, True, True, True, 42), unlocked
            connection.execute("SELECT pg_advisory_lock(305)")
            unlocked_all = connection.execute("SELECT pg_advisory_unlock_all(),lock_receipt()").fetchone()
            assert unlocked_all == ("", 42), unlocked_all
            print(json.dumps(dict(protocol=protocol, shape="result_order", first=1,
                                  failed_row_sequence=2, written=[3,4], record_error="22003", empty=empty,
                                  unlocked=unlocked, unlocked_all=unlocked_all), sort_keys=True))
        finally:
            connection.execute("DROP SCHEMA sql_result_order_driver CASCADE")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("host")
    parser.add_argument("port", type=int)
    parser.add_argument("--user", required=True)
    args = parser.parse_args()
    for protocol in ("simple", "extended"):
        for shape in ("scalar", "table", "timezone", "initializer", "domain",
                      "sql_scalar", "sql_table", "sql_atomic", "sql_write", "sql_return_domain", "sql_table_domain"):
            exercise(args.host, args.port, args.user, protocol, shape)
        result_order(args.host, args.port, args.user, protocol)


if __name__ == "__main__":
    main()
