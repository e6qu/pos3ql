#!/usr/bin/env python3
"""Compare an existing connection's role identity with PostgreSQL."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import time

import psycopg


OLD = "actor_source_old"
RENAMED = "actor_source_renamed"
TARGET = "actor_target_old"
TARGET_RENAMED = "actor_target_renamed"
FUNCTION = "actor_create_schema"
PUBLICATION = "actor_source_publication"
PUBLICATION_OWNER = "actor_publication_owner"
GRANT_GROUP = "actor_grant_group"
GRANTOR = "actor_grantor"
GRANT_MEMBER = "actor_grant_member"
EFFECTIVE_ROLE = "actor_effective_role"
PENDING_TABLE = "actor_source_pending"
TABLES = ("actor_source_owned", "actor_source_stale", "actor_source_fresh", PENDING_TABLE)


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
            original_oid = admin.execute("SELECT oid FROM pg_roles WHERE rolname = %s", (OLD,)).fetchone()[0]
            admin.execute(f"GRANT CREATE ON SCHEMA public TO {OLD}")
            with psycopg.connect(**settings, user=OLD) as actor:
                actor.execute("SET statement_timeout = '5s'")
                actor_pid = actor.execute("SELECT pg_backend_pid()").fetchone()[0]

                def observe(label, statement):
                    value = result(actor, statement)
                    observations.append([label, value])
                    print(f"{port} {label}: {value}", flush=True)

                def change_with_pending_creation(label, statement, revoke_create):
                    actor.execute("BEGIN")
                    actor.execute(f"CREATE TABLE public.{PENDING_TABLE} (id integer)")
                    if revoke_create:
                        admin.execute(f"REVOKE ALL ON SCHEMA public FROM {RENAMED}")
                    admin_pid = admin.execute("SELECT pg_backend_pid()").fetchone()[0]
                    with psycopg.connect(**settings, user=administrator) as observer:
                        observer.execute("SET statement_timeout = '5s'")
                        with ThreadPoolExecutor(max_workers=1) as executor:
                            future = executor.submit(result, admin, statement)
                            waited = False
                            try:
                                deadline = time.monotonic() + 4
                                while time.monotonic() < deadline and not future.done():
                                    waiting = observer.execute("SELECT wait_event_type FROM pg_stat_activity WHERE pid = %s", (admin_pid,)).fetchone()
                                    if waiting and waiting[0] == "Lock":
                                        waited = True
                                        break
                                    time.sleep(0.01)
                            finally:
                                actor.execute("ROLLBACK")
                            observations.append([label + "_waited", ["ok", [(waited,)]]])
                            observations.append([label + "_result", future.result(timeout=5)])

                observe("login", "SELECT current_user::text, session_user::text")
                change_with_pending_creation("rename_pending_creator", f"ALTER ROLE {OLD} RENAME TO {RENAMED}", False)
                admin.execute(f"CREATE ROLE {OLD} LOGIN")
                observe("renamed_login", "SELECT current_user::text, session_user::text")
                observations.append(["renamed_activity_user", result(admin, f"SELECT usename::text FROM pg_stat_activity WHERE pid = {actor_pid}")])
                observations.append(["renamed_activity_identity", result(admin, f"SELECT usesysid = {original_oid} FROM pg_stat_activity WHERE pid = {actor_pid}")])
                observe("reset_role", "RESET ROLE")
                observe("role_after_reset", "SELECT current_user::text, session_user::text")
                observe("reset_authorization", "RESET SESSION AUTHORIZATION")
                observe("authorization_after_reset", "SELECT current_user::text, session_user::text")
                admin.execute(f"CREATE ROLE {TARGET}")
                admin.execute(f"GRANT {TARGET} TO {RENAMED}")
                observe("select_member_role", f"SET ROLE {TARGET}")
                observe("selected_role", "SELECT current_user::text, session_user::text")
                admin.execute(f"ALTER ROLE {TARGET} RENAME TO {TARGET_RENAMED}")
                admin.execute(f"CREATE ROLE {TARGET}")
                observe("renamed_selected_role", "SELECT current_user::text, session_user::text")
                observe("begin_role_transaction", "BEGIN")
                observe("role_savepoint", "SAVEPOINT role_source")
                observe("reset_role_in_savepoint", "RESET ROLE")
                observe("reset_role_identity", "SELECT current_user::text, session_user::text")
                observe("restore_role_savepoint", "ROLLBACK TO role_source")
                observe("restored_role_identity", "SELECT current_user::text, session_user::text")
                observe("commit_role_transaction", "COMMIT")
                observe("return_to_login_role", f"SET ROLE {RENAMED}")
                observe("login_role_identity", "SELECT current_user::text, session_user::text")
                admin.execute(f"GRANT CREATE ON DATABASE postgres TO {TARGET_RENAMED}")
                admin.execute(f"CREATE FUNCTION public.{FUNCTION}() RETURNS void LANGUAGE plpgsql SECURITY DEFINER AS $$ BEGIN EXECUTE 'CREATE SCHEMA AUTHORIZATION CURRENT_USER'; END $$")
                admin.execute(f"ALTER FUNCTION public.{FUNCTION}() OWNER TO {TARGET_RENAMED}")
                observe("schema_as_function_owner", f"SELECT public.{FUNCTION}()")
                observations.append(["created_schema_owner", result(admin, f"SELECT namespace.nspname::text, owner.rolname::text FROM pg_namespace namespace JOIN pg_roles owner ON owner.oid = namespace.nspowner WHERE namespace.nspname = '{TARGET_RENAMED}'")])
                admin.execute(f"CREATE ROLE {PUBLICATION_OWNER}")
                admin.execute(f"GRANT CREATE ON DATABASE postgres TO {PUBLICATION_OWNER}")
                admin.execute(f"CREATE PUBLICATION {PUBLICATION}")
                admin.execute(f"ALTER PUBLICATION {PUBLICATION} OWNER TO {PUBLICATION_OWNER}")
                admin.execute(f"REVOKE ALL ON DATABASE postgres FROM {PUBLICATION_OWNER}")
                observations.append(["publication_prevents_owner_deletion", result(admin, f"DROP ROLE {PUBLICATION_OWNER}")])
                observe("create_as_original", f"CREATE TABLE public.{TABLES[0]} (id integer)")
                observations.append([
                    "created_owner",
                    result(admin, f"SELECT tableowner::text FROM pg_tables WHERE schemaname = 'public' AND tablename = '{TABLES[0]}'"),
                ])
                admin.execute(f"DROP TABLE IF EXISTS public.{TABLES[0]}")
                admin.execute(f"REVOKE {TARGET_RENAMED} FROM {RENAMED}")
                change_with_pending_creation("drop_pending_creator", f"DROP ROLE {RENAMED}", True)
                admin.execute(f"CREATE ROLE {RENAMED} LOGIN")
                admin.execute(f"GRANT CREATE ON SCHEMA public TO {OLD}, {RENAMED}")
                admin.execute(f"GRANT {TARGET_RENAMED} TO {OLD}, {RENAMED}")
                observe("constant_after_replacement", "SELECT 1")
                observations.append(["replaced_activity_user", result(admin, f"SELECT usename::text FROM pg_stat_activity WHERE pid = {actor_pid}")])
                observations.append(["replacement_has_different_identity", result(admin, f"SELECT activity.usesysid <> role.oid FROM pg_stat_activity activity JOIN pg_roles role ON role.rolname = '{RENAMED}' WHERE activity.pid = {actor_pid}")])
                observe("user_after_replacement", "SELECT current_user::text")
                observe("session_after_replacement", "SELECT session_user::text")
                observe("create_after_replacement", f"CREATE TABLE public.{TABLES[1]} (id integer)")
                observe("membership_after_replacement", f"SET ROLE {TARGET_RENAMED}")
                observe("reset_replaced_role", "RESET ROLE")
                observe("reset_replaced_authorization", "RESET SESSION AUTHORIZATION")
                observe("identity_after_replaced_resets", "SELECT current_user::text")
                observe("discard_replaced_identity", "DISCARD ALL")
                observe("constant_after_discard", "SELECT 1")
                observe("identity_after_discard", "SELECT current_user::text")
                with psycopg.connect(**settings, user=RENAMED) as fresh:
                    fresh.execute("SET statement_timeout = '5s'")
                    observations.append(["fresh_user", result(fresh, "SELECT current_user::text, session_user::text")])
                    observations.append(["fresh_create", result(fresh, f"CREATE TABLE public.{TABLES[2]} (id integer)")])
                    observations.append(["replacement_cannot_read_old_query", result(fresh, f"SELECT query = '<insufficient privilege>' FROM pg_stat_activity WHERE pid = {actor_pid}")])
                    observations.append(["replacement_cannot_cancel_old_connection", result(fresh, f"SELECT pg_cancel_backend({actor_pid})")])
                for role in (GRANT_GROUP, GRANTOR, GRANT_MEMBER):
                    admin.execute(f"CREATE ROLE {role}")
                admin.execute(f"GRANT {GRANT_GROUP} TO {GRANTOR} WITH ADMIN OPTION")
                admin.execute(f"SET ROLE {GRANTOR}")
                admin.execute(f"GRANT {GRANT_GROUP} TO {GRANT_MEMBER}")
                admin.execute("RESET ROLE")
                observations.append(["grantor_dependency", result(admin, f"DROP ROLE {GRANTOR}")])
                observations.append(["drop_grantor_and_member", result(admin, f"DROP ROLE {GRANTOR}, {GRANT_MEMBER}")])
                admin.execute(f"CREATE ROLE {EFFECTIVE_ROLE} CREATEROLE")
                admin.execute(f"SET ROLE {EFFECTIVE_ROLE}")
                observations.append(["effective_role_cannot_drop_itself", result(admin, f"DROP ROLE {EFFECTIVE_ROLE}")])
                admin.execute("RESET ROLE")
        finally:
            admin.execute("RESET ROLE")
            admin.execute(f"DROP ROLE IF EXISTS {GRANTOR}, {GRANT_MEMBER}, {GRANT_GROUP}, {EFFECTIVE_ROLE}")
            admin.execute(f"DROP PUBLICATION IF EXISTS {PUBLICATION}")
            if admin.execute("SELECT 1 FROM pg_roles WHERE rolname = %s", (PUBLICATION_OWNER,)).fetchone():
                admin.execute(f"REVOKE ALL ON DATABASE postgres FROM {PUBLICATION_OWNER}")
                admin.execute(f"DROP ROLE {PUBLICATION_OWNER}")
            admin.execute(f"DROP FUNCTION IF EXISTS public.{FUNCTION}()")
            admin.execute(f"DROP SCHEMA IF EXISTS {TARGET_RENAMED}")
            for table in TABLES:
                admin.execute(f"DROP TABLE IF EXISTS public.{table}")
            for role in (OLD, RENAMED):
                if admin.execute("SELECT 1 FROM pg_roles WHERE rolname = %s", (role,)).fetchone():
                    admin.execute(f"REVOKE ALL ON SCHEMA public FROM {role}")
                    if admin.execute("SELECT 1 FROM pg_roles WHERE rolname = %s", (TARGET_RENAMED,)).fetchone():
                        admin.execute(f"REVOKE {TARGET_RENAMED} FROM {role}")
                    admin.execute(f"DROP ROLE {role}")
            for role in (TARGET, TARGET_RENAMED):
                if admin.execute("SELECT 1 FROM pg_roles WHERE rolname = %s", (role,)).fetchone():
                    admin.execute(f"REVOKE ALL ON DATABASE postgres FROM {role}")
                admin.execute(f"DROP ROLE IF EXISTS {role}")
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
    if actual != reference:
        for expected, observed in zip(reference, actual, strict=True):
            if expected != observed:
                print(f"MISMATCH: PostgreSQL {expected!r}; pos3ql {observed!r}", flush=True)
        raise AssertionError("existing sessions changed role identity after rename or replacement")
    print("PASS: connection role identity, renames, resets, replacement and creation ownership")


if __name__ == "__main__":
    main()
