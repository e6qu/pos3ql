#!/usr/bin/env python3
"""Compare PostgreSQL and pos3ql subscribers' replica-role apply behavior.

PostgreSQL's apply worker runs with session_replication_role = replica
(REL_18_6/src/backend/replication/logical/worker.c). Triggers then fire only
when enabled REPLICA or ALWAYS (commands/trigger.c, TriggerEnabled), so the
ORIGIN-enabled RI triggers behind foreign keys skip their checks and actions:
an orphan child row that arrives before, or without, its parent is stored, and
a replicated parent DELETE cascades nothing. Apply writes rows directly
(executor/execReplication.c), so rewrite rules never fire. Concrete expected
rows prevent two equally delayed subscribers from passing before apply ends.
"""

import argparse
import time

import psycopg


def connect(port):
    return psycopg.connect(host="127.0.0.1", port=port, user="postgres", dbname="postgres", autocommit=True)


def snapshot(connection):
    parents = connection.execute("SELECT id FROM replica_role_parent ORDER BY id").fetchall()
    children = connection.execute("SELECT id, parent FROM replica_role_child ORDER BY id").fetchall()
    audit = connection.execute("SELECT event, trigger_name, id FROM replica_role_audit ORDER BY trigger_name, event, id").fetchall()
    rules = connection.execute("SELECT id FROM replica_role_rule_log ORDER BY id").fetchall()
    return parents, children, audit, rules


def wait_for(label, connection, expected):
    parents, children, audit, rules = expected
    expected = parents, children, sorted(audit, key=lambda row: (row[1], row[0], row[2])), rules
    deadline = time.monotonic() + 10
    while True:
        actual = snapshot(connection)
        if actual == expected:
            return actual
        if time.monotonic() >= deadline:
            raise AssertionError(f"{label}: expected {expected!r}, got {actual!r}")
        time.sleep(0.1)


PUBLISHED_TABLES = [
    "CREATE TABLE replica_role_parent(id integer PRIMARY KEY)",
    "CREATE TABLE replica_role_child(id integer PRIMARY KEY, parent integer)",
]


def setup_subscriber(connection):
    for statement in [
        "CREATE TABLE replica_role_parent(id integer PRIMARY KEY)",
        "CREATE TABLE replica_role_child(id integer PRIMARY KEY, parent integer REFERENCES replica_role_parent ON DELETE CASCADE)",
        "CREATE TABLE replica_role_audit(event text, trigger_name text, id integer)",
        "CREATE TABLE replica_role_rule_log(id integer)",
        """CREATE FUNCTION replica_role_note() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN
            INSERT INTO public.replica_role_audit VALUES (TG_OP, TG_NAME, CASE WHEN TG_OP = 'DELETE' THEN OLD.id ELSE NEW.id END);
            RETURN NULL;
        END$$""",
        "CREATE TRIGGER origin_note AFTER INSERT OR UPDATE OR DELETE ON replica_role_child FOR EACH ROW EXECUTE FUNCTION replica_role_note()",
        "CREATE TRIGGER replica_note AFTER INSERT OR UPDATE OR DELETE ON replica_role_child FOR EACH ROW EXECUTE FUNCTION replica_role_note()",
        "CREATE TRIGGER always_note AFTER INSERT OR UPDATE OR DELETE ON replica_role_child FOR EACH ROW EXECUTE FUNCTION replica_role_note()",
        "CREATE TRIGGER disabled_note AFTER INSERT OR UPDATE OR DELETE ON replica_role_child FOR EACH ROW EXECUTE FUNCTION replica_role_note()",
        "ALTER TABLE replica_role_child ENABLE REPLICA TRIGGER replica_note",
        "ALTER TABLE replica_role_child ENABLE ALWAYS TRIGGER always_note",
        "ALTER TABLE replica_role_child DISABLE TRIGGER disabled_note",
        "CREATE RULE replica_role_always_rule AS ON INSERT TO replica_role_parent DO ALSO INSERT INTO replica_role_rule_log VALUES (NEW.id)",
        "ALTER TABLE replica_role_parent ENABLE ALWAYS RULE replica_role_always_rule",
    ]:
        connection.execute(statement)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--publisher", type=int, required=True)
    parser.add_argument("--control", type=int, required=True)
    parser.add_argument("--candidate", type=int, required=True)
    args = parser.parse_args()
    with connect(args.publisher) as publisher, connect(args.control) as control, connect(args.candidate) as candidate:
        print("Publisher:", publisher.execute("SELECT version()").fetchone()[0])
        print("Control:", control.execute("SELECT version()").fetchone()[0])
        for statement in PUBLISHED_TABLES:
            publisher.execute(statement)
        publisher.execute("INSERT INTO replica_role_parent VALUES (1)")
        publisher.execute("INSERT INTO replica_role_child VALUES (10, 1), (11, 2)")
        publisher.execute("CREATE PUBLICATION replica_role_pub FOR TABLE replica_role_parent, replica_role_child")
        subscriptions = [(control, "replica_role_control"), (candidate, "replica_role_candidate")]
        created = []
        try:
            for connection, name in subscriptions:
                setup_subscriber(connection)
                connection.execute(f"CREATE SUBSCRIPTION {name} CONNECTION 'host=127.0.0.1 port={args.publisher} user=postgres dbname=postgres sslmode=disable' PUBLICATION replica_role_pub")
                created.append((connection, name))

            def compare(label, expected):
                wanted = wait_for("PostgreSQL " + label, control, expected)
                actual = wait_for("pos3ql " + label, candidate, expected)
                assert actual == wanted, (label, wanted, actual)
                print(f"PASS: replica role {label}: {actual!r}", flush=True)

            audit = [
                ("INSERT", "always_note", 10),
                ("INSERT", "always_note", 11),
                ("INSERT", "replica_note", 10),
                ("INSERT", "replica_note", 11),
            ]
            compare("initial COPY keeps an orphan", ([(1,)], [(10, 1), (11, 2)], audit, []))
            for connection, name in subscriptions:
                deadline = time.monotonic() + 10
                while True:
                    states = connection.execute("SELECT r.srsubstate::text FROM pg_subscription_rel r JOIN pg_subscription s ON s.oid = r.srsubid WHERE s.subname = %s", (name,), prepare=False).fetchall()
                    if states == [("r",), ("r",)]:
                        break
                    if time.monotonic() >= deadline:
                        raise AssertionError(f"{name}: tables did not become ready: {states!r}")
                    time.sleep(0.1)
            with publisher.transaction():
                publisher.execute("INSERT INTO replica_role_child VALUES (12, 3)")
                publisher.execute("INSERT INTO replica_role_parent VALUES (3)")
            audit += [("INSERT", "always_note", 12), ("INSERT", "replica_note", 12)]
            compare("child before its parent", ([(1,), (3,)], [(10, 1), (11, 2), (12, 3)], audit, []))
            publisher.execute("DELETE FROM replica_role_parent WHERE id = 1")
            compare("parent DELETE cascades nothing", ([(3,)], [(10, 1), (11, 2), (12, 3)], audit, []))
            publisher.execute("UPDATE replica_role_child SET parent = 4 WHERE id = 12")
            audit += [("UPDATE", "always_note", 12), ("UPDATE", "replica_note", 12)]
            compare("UPDATE to a missing parent", ([(3,)], [(10, 1), (11, 2), (12, 4)], audit, []))
            publisher.execute("DELETE FROM replica_role_child WHERE id = 11")
            audit += [("DELETE", "always_note", 11), ("DELETE", "replica_note", 11)]
            compare("replicated DELETE", ([(3,)], [(10, 1), (12, 4)], audit, []))
            for connection in (control, candidate):
                connection.execute("INSERT INTO replica_role_parent VALUES (50)")
                connection.execute("INSERT INTO replica_role_child VALUES (50, 50)")
                connection.execute("DELETE FROM replica_role_child WHERE id = 50")
                connection.execute("DELETE FROM replica_role_parent WHERE id = 50")
            local = [
                ("DELETE", "always_note", 50),
                ("DELETE", "origin_note", 50),
                ("INSERT", "always_note", 50),
                ("INSERT", "origin_note", 50),
            ]
            compare("local writes fire ORIGIN triggers and rules", ([(3,)], [(10, 1), (12, 4)], audit + local, [(50,)]))
        finally:
            for connection, name in reversed(created):
                connection.execute(f"DROP SUBSCRIPTION {name}")
            for connection, _ in subscriptions:
                connection.execute("DROP TABLE IF EXISTS replica_role_child, replica_role_parent, replica_role_audit, replica_role_rule_log")
                connection.execute("DROP FUNCTION IF EXISTS replica_role_note()")
            publisher.execute("DROP PUBLICATION replica_role_pub")
            publisher.execute("DROP TABLE replica_role_child, replica_role_parent")


if __name__ == "__main__":
    main()
