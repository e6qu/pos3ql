#!/usr/bin/env python3
"""Compare database-local type moves with PostgreSQL using two databases."""
import argparse
from pathlib import Path

import psycopg
from psycopg import sql

ROOT = Path(__file__).resolve().parent
SETUP = ROOT.joinpath("type_reference_setup.sql").read_text()
MOVES = ROOT.joinpath("type_reference_moves.sql").read_text()
SNAPSHOT = ROOT.joinpath("type_reference_snapshot.sql").read_text()


def execute(connection, statements):
    rows = []
    with connection.cursor() as cursor:
        cursor.execute(statements, prepare=False)
        while True:
            if cursor.description:
                rows.append(cursor.fetchall())
            if not cursor.nextset():
                break
    return rows


def exercise(host, port, user):
    settings = dict(host=host, port=port, user=user, autocommit=True)
    names = ("type_reference_first", "type_reference_second")
    with psycopg.connect(**settings, dbname="postgres") as admin:
        try:
            for name in names:
                admin.execute(sql.SQL("DROP DATABASE IF EXISTS {}").format(sql.Identifier(name)))
                admin.execute(
                    sql.SQL("CREATE DATABASE {} TEMPLATE template0").format(sql.Identifier(name))
                )
            with psycopg.connect(**settings, dbname=names[0]) as first:
                with psycopg.connect(**settings, dbname=names[1]) as second:
                    execute(first, SETUP)
                    execute(second, SETUP)
                    execute(second, "DROP TYPE type_scope.drop_enum")
                    assert execute(first, "SELECT 'ready'::type_scope.drop_enum::text") == [[("ready",)]]
                    assert execute(
                        second, "SELECT count(*) FROM pg_type WHERE typname = 'drop_enum'"
                    ) == [[(0,)]]
                    original = execute(first, SNAPSHOT)
                    untouched = execute(second, SNAPSHOT)
                    execute(first, "BEGIN; " + MOVES)
                    assert execute(second, SNAPSHOT) == untouched, "pending move escaped its database"
                    execute(first, "ROLLBACK")
                    assert execute(first, SNAPSHOT) == original, "rollback changed type references"
                    execute(first, "BEGIN; " + MOVES + " COMMIT;")
                    published = execute(first, SNAPSHOT)
                    assert execute(second, SNAPSHOT) == untouched, "committed move escaped its database"
                    assert execute(
                        second, "SELECT type_scope.echo_domain(7), 7::type_scope.child_domain"
                    ) == [[(7, 7)]]
                    return original, published, untouched
        finally:
            for name in names:
                admin.execute(sql.SQL("DROP DATABASE IF EXISTS {}").format(sql.Identifier(name)))


def exercise_enum_snapshot(host, port, user):
    """Catalog label changes must not depend on a row's transaction version."""
    settings = dict(host=host, port=port, user=user, autocommit=True, connect_timeout=5)
    name = "enum_catalog_snapshot"
    print(f"Enum snapshot setup on port {port}", flush=True)
    with psycopg.connect(**settings, dbname="postgres") as admin:
        admin.execute(sql.SQL("DROP DATABASE IF EXISTS {}").format(sql.Identifier(name)))
        admin.execute(sql.SQL("CREATE DATABASE {} TEMPLATE template0").format(sql.Identifier(name)))
        try:
            with psycopg.connect(**settings, dbname=name) as writer:
                with psycopg.connect(**settings, dbname=name) as reader:
                    execute(writer, "SET statement_timeout = '5s'")
                    execute(reader, "SET statement_timeout = '5s'")
                    execute(
                        writer,
                        "CREATE TYPE status AS ENUM ('old', 'ready'); "
                        "CREATE TABLE orders (id integer PRIMARY KEY, state status); "
                        "INSERT INTO orders VALUES (1, 'old')",
                    )
                    print(f"Enum snapshot rename on port {port}", flush=True)
                    execute(reader, "BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT id FROM orders")
                    execute(writer, "ALTER TYPE status RENAME VALUE 'old' TO 'new'")
                    renamed = execute(reader, "SELECT state::text FROM orders WHERE id = 1")
                    execute(reader, "ROLLBACK")
                    print(f"Enum snapshot renumbering on port {port}", flush=True)
                    execute(writer, "CREATE TYPE ordered_status AS ENUM ('low', 'high'); "
                            "CREATE TABLE ordered_orders (id integer PRIMARY KEY, state ordered_status); "
                            "INSERT INTO ordered_orders VALUES (1, 'high')")
                    execute(reader, "BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT id FROM ordered_orders")
                    for index in range(30):
                        execute(writer, f"ALTER TYPE ordered_status ADD VALUE 'middle_{index}' BEFORE 'high'")
                    renumbered = execute(
                        reader,
                        "SELECT state::text, state = 'high'::ordered_status, "
                        "state > 'middle_29'::ordered_status "
                        "FROM ordered_orders WHERE id = 1",
                    )
                    execute(reader, "ROLLBACK")
                    return renamed, renumbered
        finally:
            admin.execute(sql.SQL("DROP DATABASE IF EXISTS {}").format(sql.Identifier(name)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--pg", type=int, required=True)
    parser.add_argument("--p3", type=int, required=True)
    parser.add_argument("--user", default="postgres")
    args = parser.parse_args()
    print("Checking PostgreSQL database-local type moves", flush=True)
    reference = exercise(args.host, args.pg, args.user), exercise_enum_snapshot(
        args.host, args.pg, args.user
    )
    print("PostgreSQL enum catalog snapshot results:", reference[1], flush=True)
    print("Checking pos3ql database-local type moves", flush=True)
    actual = exercise(args.host, args.p3, args.user), exercise_enum_snapshot(
        args.host, args.p3, args.user
    )
    assert actual == reference, (actual, reference)
    print("PASS: database-local type moves, rollback, columns, signatures and comments")


if __name__ == "__main__":
    main()
