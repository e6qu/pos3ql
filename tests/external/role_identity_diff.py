#!/usr/bin/env python3
"""Compare an existing connection's role identity with PostgreSQL."""

import argparse
import json
from pathlib import Path

import psycopg


OLD = "actor_source_old"
RENAMED = "actor_source_renamed"
TABLES = ("actor_source_owned", "actor_source_stale", "actor_source_fresh")


def result(connection, statement):
    try:
        with connection.cursor() as cursor:
            cursor.execute(statement, prepare=False)
            return ["ok", cursor.fetchall() if cursor.description else []]
    except psycopg.Error as error:
        return ["error", error.sqlstate]


def exercise(host, port, administrator):
    settings = dict(host=host, port=port, dbname="postgres", autocommit=True, connect_timeout=5)
    observations = []
    with psycopg.connect(**settings, user=administrator) as admin:
        admin.execute("SET statement_timeout = '5s'")
        try:
            admin.execute(f"CREATE ROLE {OLD} LOGIN")
            admin.execute(f"GRANT CREATE ON SCHEMA public TO {OLD}")
            with psycopg.connect(**settings, user=OLD) as actor:
                actor.execute("SET statement_timeout = '5s'")

                def observe(label, statement):
                    value = result(actor, statement)
                    observations.append([label, value])
                    print(f"{port} {label}: {value}", flush=True)

                observe("login", "SELECT current_user::text, session_user::text")
                admin.execute(f"ALTER ROLE {OLD} RENAME TO {RENAMED}")
                admin.execute(f"CREATE ROLE {OLD} LOGIN")
                observe("renamed_login", "SELECT current_user::text, session_user::text")
                observe("reset_role", "RESET ROLE")
                observe("role_after_reset", "SELECT current_user::text, session_user::text")
                observe("reset_authorization", "RESET SESSION AUTHORIZATION")
                observe("authorization_after_reset", "SELECT current_user::text, session_user::text")
                observe("create_as_original", f"CREATE TABLE public.{TABLES[0]} (id integer)")
                observations.append([
                    "created_owner",
                    result(admin, f"SELECT tableowner::text FROM pg_tables WHERE schemaname = 'public' AND tablename = '{TABLES[0]}'"),
                ])
                admin.execute(f"DROP TABLE IF EXISTS public.{TABLES[0]}")
                admin.execute(f"REVOKE ALL ON SCHEMA public FROM {RENAMED}")
                admin.execute(f"DROP ROLE {RENAMED}")
                admin.execute(f"CREATE ROLE {RENAMED} LOGIN")
                admin.execute(f"GRANT CREATE ON SCHEMA public TO {OLD}, {RENAMED}")
                observe("constant_after_replacement", "SELECT 1")
                observe("user_after_replacement", "SELECT current_user::text")
                observe("session_after_replacement", "SELECT session_user::text")
                observe("create_after_replacement", f"CREATE TABLE public.{TABLES[1]} (id integer)")
                with psycopg.connect(**settings, user=RENAMED) as fresh:
                    fresh.execute("SET statement_timeout = '5s'")
                    observations.append(["fresh_user", result(fresh, "SELECT current_user::text, session_user::text")])
                    observations.append(["fresh_create", result(fresh, f"CREATE TABLE public.{TABLES[2]} (id integer)")])
        finally:
            for table in TABLES:
                admin.execute(f"DROP TABLE IF EXISTS public.{table}")
            for role in (OLD, RENAMED):
                if admin.execute("SELECT 1 FROM pg_roles WHERE rolname = %s", (role,)).fetchone():
                    admin.execute(f"REVOKE ALL ON SCHEMA public FROM {role}")
                    admin.execute(f"DROP ROLE {role}")
    return observations


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", required=True)
    parser.add_argument("--pg", type=int, required=True)
    parser.add_argument("--p3", type=int, required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    reference = exercise(args.host, args.pg, args.user)
    evidence = {"postgresql": reference}
    args.output.write_text(json.dumps(evidence, indent=2) + "\n")
    try:
        actual = exercise(args.host, args.p3, args.user)
    except Exception as error:
        evidence["pos3ql_error"] = str(error)
        args.output.write_text(json.dumps(evidence, indent=2) + "\n")
        raise
    evidence["pos3ql"] = actual
    args.output.write_text(json.dumps(evidence, indent=2) + "\n")
    assert actual == reference, "existing sessions changed role identity after rename or replacement"
    print("PASS: connection role identity, renames, resets, replacement and creation ownership")


if __name__ == "__main__":
    main()
