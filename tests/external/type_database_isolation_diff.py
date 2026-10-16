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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--pg", type=int, required=True)
    parser.add_argument("--p3", type=int, required=True)
    parser.add_argument("--user", default="postgres")
    args = parser.parse_args()
    reference = exercise(args.host, args.pg, args.user)
    actual = exercise(args.host, args.p3, args.user)
    assert actual == reference, (actual, reference)
    print("PASS: database-local type moves, rollback, columns, signatures and comments")


if __name__ == "__main__":
    main()
