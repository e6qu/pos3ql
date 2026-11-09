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
        admin.execute("CREATE SEQUENCE issued")
        admin.execute("CREATE TABLE orders(id integer,payload text)")
        admin.execute("CREATE TABLE audit(stage text,n bigint,payload text)")
        if shape == "scalar":
            signature = "RETURNS text"
            finish = "RETURN saved||':'||n::text;"
            call = "SELECT waiting(NEW.payload) INTO answer"
        else:
            signature = "RETURNS TABLE(payload text)"
            finish = "payload:=saved; RETURN NEXT; payload:=saved||':'||n::text; RETURN NEXT; RETURN;"
            call = "SELECT string_agg(payload,',') INTO answer FROM waiting(NEW.payload)"
        admin.execute(f"""CREATE FUNCTION waiting(p text) {signature} LANGUAGE plpgsql AS $$
            DECLARE n bigint:=nextval('issued'); saved text:=p;
            BEGIN
                INSERT INTO audit VALUES('before',n,saved);
                PERFORM pg_advisory_xact_lock(846201);
                INSERT INTO audit VALUES('after',n,saved);
                {finish}
            END$$""")
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

                def insert():
                    try:
                        with writer.cursor() as cursor:
                            cursor.execute("INSERT INTO orders VALUES(%s,%s) RETURNING id",
                                           (order, "x" * 8192))
                            outcome.append(("rows", cursor.fetchall()))
                    except psycopg.Error as error:
                        outcome.append(("error", error.sqlstate))

                worker = threading.Thread(target=insert, daemon=True)
                worker.start()
                deadline = time.monotonic() + 5
                try:
                    while time.monotonic() < deadline:
                        blockers = admin.execute("SELECT pg_blocking_pids(%s)", (pid,)).fetchone()[0]
                        if blockers:
                            break
                        assert worker.is_alive(), outcome
                        time.sleep(0.01)
                    else:
                        raise AssertionError("function never reached its held lock")
                    assert admin.execute("SELECT 40+2").fetchone() == (42,)
                    assert admin.execute("SELECT last_value FROM issued").fetchone() == (order,)
                    assert admin.execute("SELECT count(*) FROM audit").fetchone() == ((order - 1) * 3,)
                    if cancel:
                        assert admin.execute("SELECT pg_cancel_backend(%s)", (pid,)).fetchone() == (True,)
                    else:
                        assert admin.execute("SELECT pg_advisory_unlock(846201)").fetchone() == (True,)
                    worker.join(timeout=5)
                    assert not worker.is_alive(), "function did not finish after release/cancellation"
                    assert outcome == ([("error", "57014")] if cancel else [("rows", [(order,)])]), outcome
                    transcript.append(outcome)
                finally:
                    admin.execute("SELECT pg_advisory_unlock_all()")
                    if worker.is_alive():
                        admin.execute("SELECT pg_cancel_backend(%s)", (pid,))
                        worker.join(timeout=5)
            writer.execute("INSERT INTO orders VALUES(%s,%s)", (3, "fresh"))
            expected_length = 8194 if shape == "scalar" else 16387
            expected_fresh = 7 if shape == "scalar" else 13
            audit = admin.execute("SELECT stage,n,length(payload) FROM audit ORDER BY n,stage").fetchall()
            assert audit == [("after", 1, 8192), ("before", 1, 8192), ("caller", 1, expected_length),
                             ("after", 3, 5), ("before", 3, 5), ("caller", 3, expected_fresh)], audit
            assert admin.execute("SELECT id FROM orders ORDER BY id").fetchall() == [(1,), (3,)]
            assert admin.execute("SELECT last_value FROM issued").fetchone() == (3,)
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
        for shape in ("scalar", "table"):
            exercise(args.host, args.port, args.user, protocol, shape)


if __name__ == "__main__":
    main()
