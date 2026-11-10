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
            if shape == "sql_return_domain":
                admin.execute(f"CREATE FUNCTION sql_check(p text) RETURNS boolean LANGUAGE plpgsql STRICT AS $$ {child} RETURN true; END$$")
                admin.execute("CREATE DOMAIN sql_checked_payload AS text CHECK(sql_check(VALUE))")
                sql_signature = "RETURNS sql_checked_payload"
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
                               "sql_write": 8192, "sql_return_domain": 8192}[shape]
            expected_fresh = {"scalar": 7, "initializer": 7, "domain": 7, "table": 13, "timezone": 22,
                              "sql_scalar": 7, "sql_atomic": 7, "sql_table": 13,
                              "sql_write": 5, "sql_return_domain": 5}[shape]
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


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("host")
    parser.add_argument("port", type=int)
    parser.add_argument("--user", required=True)
    args = parser.parse_args()
    for protocol in ("simple", "extended"):
        for shape in ("scalar", "table", "timezone", "initializer", "domain",
                      "sql_scalar", "sql_table", "sql_atomic", "sql_write", "sql_return_domain"):
            exercise(args.host, args.port, args.user, protocol, shape)


if __name__ == "__main__":
    main()
