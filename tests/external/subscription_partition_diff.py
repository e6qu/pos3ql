#!/usr/bin/env python3
"""Compare actual PostgreSQL and pos3ql subscribers with reordered partitions.

The publisher and control use the PostgreSQL 18 image pinned by CI. Partition
move events follow REL_18_6/src/backend/replication/logical/worker.c and generated
values follow REL_18_6/src/backend/executor/execReplication.c. Concrete expected
rows prevent two equally delayed subscribers from passing before apply finishes.
"""

import argparse
import time

import psycopg


def connect(port):
    return psycopg.connect(host="127.0.0.1", port=port, user="postgres", dbname="postgres", autocommit=True)


def snapshot(connection):
    rows = connection.execute("SELECT id, label, computed FROM subscription_layout_diff ORDER BY id").fetchall()
    audit = connection.execute("SELECT event, leaf, id, label, computed FROM subscription_layout_audit_diff ORDER BY event, leaf, id, label").fetchall()
    return rows, audit


def wait_for(label, connection, rows, audit):
    expected = rows, sorted(audit)
    deadline = time.monotonic() + 10
    while True:
        actual = snapshot(connection)
        if actual == expected:
            return actual
        if time.monotonic() >= deadline:
            raise AssertionError(f"{label}: expected {expected!r}, got {actual!r}")
        time.sleep(0.1)


def setup_subscriber(connection):
    for statement in [
        "CREATE TABLE subscription_layout_diff(id integer PRIMARY KEY, label text, computed integer GENERATED ALWAYS AS (length(label)) STORED) PARTITION BY RANGE(id)",
        "CREATE TABLE subscription_layout_low_diff(label text, computed integer GENERATED ALWAYS AS (length(label) + 100) STORED, id integer NOT NULL)",
        "CREATE TABLE subscription_layout_high_diff(computed integer GENERATED ALWAYS AS (length(label) + 200) STORED, id integer NOT NULL, label text)",
        "ALTER TABLE subscription_layout_diff ATTACH PARTITION subscription_layout_low_diff FOR VALUES FROM (0) TO (10)",
        "ALTER TABLE subscription_layout_diff ATTACH PARTITION subscription_layout_high_diff FOR VALUES FROM (10) TO (20)",
        "CREATE TABLE subscription_layout_audit_diff(event text, leaf text, id integer, label text, computed integer)",
        """CREATE FUNCTION subscription_layout_before_diff() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN
            IF TG_OP = 'INSERT' THEN NEW.label := upper(NEW.label);
            ELSE NEW.label := NEW.label || ':U'; END IF;
            RETURN NEW;
        END$$""",
        """CREATE FUNCTION subscription_layout_after_diff() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN
            IF TG_OP = 'DELETE' THEN INSERT INTO public.subscription_layout_audit_diff VALUES (TG_OP, TG_TABLE_NAME, OLD.id, OLD.label, OLD.computed);
            ELSE INSERT INTO public.subscription_layout_audit_diff VALUES (TG_OP, TG_TABLE_NAME, NEW.id, NEW.label, NEW.computed); END IF;
            RETURN NULL;
        END$$""",
        "CREATE TRIGGER subscription_layout_before_diff BEFORE INSERT OR UPDATE ON subscription_layout_diff FOR EACH ROW EXECUTE FUNCTION subscription_layout_before_diff()",
        "CREATE TRIGGER subscription_layout_after_diff AFTER INSERT OR UPDATE OR DELETE ON subscription_layout_diff FOR EACH ROW EXECUTE FUNCTION subscription_layout_after_diff()",
    ]:
        connection.execute(statement)
    for leaf in ("low", "high"):
        for trigger in ("before", "after"):
            connection.execute(f"ALTER TABLE subscription_layout_{leaf}_diff ENABLE REPLICA TRIGGER subscription_layout_{trigger}_diff")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--publisher", type=int, required=True)
    parser.add_argument("--control", type=int, required=True)
    parser.add_argument("--candidate", type=int, required=True)
    args = parser.parse_args()
    with connect(args.publisher) as publisher, connect(args.control) as control, connect(args.candidate) as candidate:
        print("Publisher:", publisher.execute("SELECT version()").fetchone()[0])
        print("Control:", control.execute("SELECT version()").fetchone()[0])
        publisher.execute("CREATE TABLE subscription_layout_diff(id integer PRIMARY KEY, label text)")
        publisher.execute("INSERT INTO subscription_layout_diff VALUES (1, 'one'), (11, 'eleven')")
        publisher.execute("CREATE PUBLICATION subscription_layout_pub_diff FOR TABLE subscription_layout_diff")
        subscriptions = [(control, "layout_control_diff"), (candidate, "layout_candidate_diff")]
        created = []
        try:
            for connection, name in subscriptions:
                setup_subscriber(connection)
                connection.execute(f"CREATE SUBSCRIPTION {name} CONNECTION 'host=127.0.0.1 port={args.publisher} user=postgres dbname=postgres sslmode=disable' PUBLICATION subscription_layout_pub_diff")
                created.append((connection, name))
            audit = [
                ("INSERT", "subscription_layout_low_diff", 1, "ONE", 103),
                ("INSERT", "subscription_layout_high_diff", 11, "ELEVEN", 206),
            ]

            def compare(label, rows):
                expected = wait_for("PostgreSQL " + label, control, rows, audit)
                actual = wait_for("pos3ql " + label, candidate, rows, audit)
                assert actual == expected, (label, expected, actual)
                print(f"PASS: subscription partition {label}: {actual!r}", flush=True)

            compare("initial COPY", [(1, "ONE", 103), (11, "ELEVEN", 206)])
            for connection, name in subscriptions:
                deadline = time.monotonic() + 10
                while True:
                    states = connection.execute("SELECT r.srsubstate::text FROM pg_subscription_rel r JOIN pg_subscription s ON s.oid = r.srsubid WHERE s.subname = %s", (name,), prepare=False).fetchall()
                    if states == [("r",)]:
                        break
                    if time.monotonic() >= deadline:
                        raise AssertionError(f"{name}: table did not become ready: {states!r}")
                    time.sleep(0.1)
            publisher.execute("UPDATE subscription_layout_diff SET id = 2, label = 'beta' WHERE id = 1")
            audit.append(("UPDATE", "subscription_layout_low_diff", 2, "beta:U", 106))
            compare("same leaf UPDATE", [(2, "beta:U", 106), (11, "ELEVEN", 206)])
            publisher.execute("UPDATE subscription_layout_diff SET id = 12, label = 'delta' WHERE id = 2")
            audit.extend([
                ("DELETE", "subscription_layout_low_diff", 2, "beta:U", 106),
                ("INSERT", "subscription_layout_high_diff", 12, "DELTA", 205),
            ])
            compare("partition move", [(11, "ELEVEN", 206), (12, "DELTA", 205)])
            publisher.execute("DELETE FROM subscription_layout_diff WHERE id = 11")
            audit.append(("DELETE", "subscription_layout_high_diff", 11, "ELEVEN", 206))
            compare("DELETE", [(12, "DELTA", 205)])
            publisher.execute("INSERT INTO subscription_layout_diff VALUES (3, 'next')")
            audit.append(("INSERT", "subscription_layout_low_diff", 3, "NEXT", 104))
            compare("stream INSERT", [(3, "NEXT", 104), (12, "DELTA", 205)])
            publisher.execute("TRUNCATE subscription_layout_diff")
            compare("TRUNCATE", [])
        finally:
            for connection, name in reversed(created):
                connection.execute(f"DROP SUBSCRIPTION {name}")
            for connection, _ in subscriptions:
                connection.execute("DROP TABLE IF EXISTS subscription_layout_diff CASCADE")
                connection.execute("DROP TABLE IF EXISTS subscription_layout_audit_diff")
                connection.execute("DROP FUNCTION IF EXISTS subscription_layout_before_diff()")
                connection.execute("DROP FUNCTION IF EXISTS subscription_layout_after_diff()")
            publisher.execute("DROP PUBLICATION subscription_layout_pub_diff")
            publisher.execute("DROP TABLE subscription_layout_diff")


if __name__ == "__main__":
    main()
