#!/usr/bin/env python3
"""Two-session MVCC and table-lock differential against PostgreSQL."""

import os
import sys

import psycopg


def connect(port: int):
    connection = psycopg.connect(
        host=os.environ.get("PGHOST", "127.0.0.1"),
        port=port,
        user=os.environ.get("PGUSER", "postgres"),
        dbname="postgres",
        autocommit=True,
    )
    # The shared test server reserves 16 plans; bound the driver's LRU cache.
    connection.prepared_max = 8
    return connection


def sqlstate(connection, statement: str) -> str:
    try:
        connection.execute(statement)
    except psycopg.Error as error:
        return error.sqlstate or "none"
    return "ok"


def on_conflict_snapshot_transcript(reader, writer) -> list[str]:
    writer.execute("CREATE TABLE conflict_snapshot(id integer PRIMARY KEY, value text)")
    writer.execute("INSERT INTO conflict_snapshot VALUES (1, 'old')")
    results = []
    try:
        reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        before = reader.execute("SELECT value FROM conflict_snapshot WHERE id = 1").fetchone()[0]
        writer.execute("UPDATE conflict_snapshot SET value = 'new' WHERE id = 1")
        writer.execute("INSERT INTO conflict_snapshot VALUES (2, 'later')")
        for label, command in [
            ("updated_nothing", "INSERT INTO conflict_snapshot VALUES (1, 'candidate') ON CONFLICT (id) DO NOTHING"),
            ("updated_update", "INSERT INTO conflict_snapshot VALUES (1, 'candidate') ON CONFLICT (id) DO UPDATE SET value = excluded.value"),
            ("inserted_nothing", "INSERT INTO conflict_snapshot VALUES (2, 'candidate') ON CONFLICT (id) DO NOTHING"),
            ("inserted_update", "INSERT INTO conflict_snapshot VALUES (2, 'candidate') ON CONFLICT (id) DO UPDATE SET value = excluded.value"),
        ]:
            reader.execute("SAVEPOINT before_conflict")
            error = sqlstate(reader, command)
            reader.execute("ROLLBACK TO SAVEPOINT before_conflict")
            after = reader.execute("SELECT value FROM conflict_snapshot WHERE id = 1").fetchone()[0]
            count = reader.execute("SELECT count(*) FROM conflict_snapshot").fetchone()[0]
            results.append(f"conflict_{label}={error},before:{before},after:{after},count:{count}")
        reader.execute("INSERT INTO conflict_snapshot VALUES (3, 'own')")
        own = reader.execute("INSERT INTO conflict_snapshot VALUES (3, 'changed') ON CONFLICT (id) DO UPDATE SET value = excluded.value RETURNING value").fetchall()
        results.append(f"conflict_prior_own={own}")
        reader.execute("ROLLBACK")
        current = reader.execute("INSERT INTO conflict_snapshot VALUES (1, 'candidate') ON CONFLICT (id) DO UPDATE SET value = conflict_snapshot.value || ':changed' RETURNING value").fetchall()
        results.append(f"conflict_current={current}")
        for label, command in [
            ("nothing", "INSERT INTO conflict_snapshot VALUES (4, 'first'), (4, 'second') ON CONFLICT (id) DO NOTHING"),
            ("update", "INSERT INTO conflict_snapshot VALUES (4, 'first'), (4, 'second') ON CONFLICT (id) DO UPDATE SET value = excluded.value"),
            ("existing_update", "INSERT INTO conflict_snapshot VALUES (1, 'first'), (1, 'second') ON CONFLICT (id) DO UPDATE SET value = excluded.value"),
        ]:
            reader.execute("BEGIN")
            error = sqlstate(reader, command)
            reader.execute("ROLLBACK")
            results.append(f"conflict_same_statement_{label}={error}")
        reader.execute("BEGIN")
        reader.execute("INSERT INTO conflict_snapshot VALUES (1, 'ignored') ON CONFLICT (id) DO UPDATE SET value = excluded.value WHERE false")
        writer.execute("SET lock_timeout = '50ms'")
        key_share = sqlstate(writer, "SELECT id FROM conflict_snapshot WHERE id = 1 FOR KEY SHARE")
        updated = sqlstate(writer, "UPDATE conflict_snapshot SET value = 'blocked' WHERE id = 1")
        unrelated = sqlstate(writer, "UPDATE conflict_snapshot SET value = 'unrelated' WHERE id = 2")
        reader.execute("ROLLBACK")
        released = sqlstate(writer, "UPDATE conflict_snapshot SET value = 'released' WHERE id = 1")
        results.append(f"conflict_where_false=key_share:{key_share},update:{updated},unrelated:{unrelated},released:{released}")
        reader.execute("BEGIN")
        reader.execute("INSERT INTO conflict_snapshot VALUES (1, 'ignored') ON CONFLICT (id) DO UPDATE SET id = excluded.id WHERE false")
        key_share = sqlstate(writer, "SELECT id FROM conflict_snapshot WHERE id = 1 FOR KEY SHARE")
        reader.execute("ROLLBACK")
        results.append(f"conflict_key_assignment=key_share:{key_share}")
        reader.execute("SET lock_timeout = '50ms'")
        writer.execute("BEGIN")
        writer.execute("UPDATE conflict_snapshot SET id = 10 WHERE id = 1")
        for label, command in [
            ("old_key", "INSERT INTO conflict_snapshot VALUES (1, 'candidate') ON CONFLICT (id) DO NOTHING"),
            ("new_key", "INSERT INTO conflict_snapshot VALUES (10, 'candidate') ON CONFLICT (id) DO NOTHING"),
            ("filtered", "INSERT INTO conflict_snapshot VALUES (1, 'candidate') ON CONFLICT (id) DO UPDATE SET value = excluded.value WHERE false"),
        ]:
            results.append(f"conflict_pending_{label}={sqlstate(reader, command)}")
        writer.execute("ROLLBACK")
        writer.execute("BEGIN")
        writer.execute("INSERT INTO conflict_snapshot VALUES (5, 'pending')")
        error = sqlstate(reader, "INSERT INTO conflict_snapshot VALUES (5, 'candidate') ON CONFLICT (id) DO UPDATE SET value = excluded.value")
        writer.execute("COMMIT")
        retry = reader.execute("INSERT INTO conflict_snapshot VALUES (5, 'retry') ON CONFLICT (id) DO UPDATE SET value = excluded.value RETURNING value").fetchall()
        results.append(f"conflict_pending_insert={error},retry:{retry}")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("ROLLBACK")
        writer.execute("DROP TABLE conflict_snapshot")


def dml_target_transcript(reader, _writer) -> list[str]:
    results = []
    cases = [
        ("before_update_changes_other", "BEFORE UPDATE", "UPDATE dml_target_rows SET value = value + 100 WHERE id = 2", "UPDATE dml_target_rows SET value = value + 1"),
        ("before_update_deletes_other", "BEFORE UPDATE", "DELETE FROM dml_target_rows WHERE id = 2", "UPDATE dml_target_rows SET value = value + 1"),
        ("before_delete_changes_other", "BEFORE DELETE", "UPDATE dml_target_rows SET value = value + 100 WHERE id = 2", "DELETE FROM dml_target_rows"),
        ("before_delete_deletes_other", "BEFORE DELETE", "DELETE FROM dml_target_rows WHERE id = 2", "DELETE FROM dml_target_rows"),
        ("after_update_changes_other", "AFTER UPDATE", "UPDATE dml_target_rows SET value = value + 100 WHERE id = 2", "UPDATE dml_target_rows SET value = value + 1"),
        ("joined_update_changes_other", "BEFORE UPDATE", "UPDATE dml_target_rows SET value = value + 100 WHERE id = 2", "UPDATE dml_target_rows SET value = dml_target_rows.value + 1 FROM (VALUES (1), (2)) AS source(id) WHERE source.id = dml_target_rows.id"),
        ("joined_delete_changes_other", "BEFORE DELETE", "UPDATE dml_target_rows SET value = value + 100 WHERE id = 2", "DELETE FROM dml_target_rows USING (VALUES (1), (2)) AS source(id) WHERE source.id = dml_target_rows.id"),
    ]
    try:
        for label, event, mutation, command in cases:
            reader.execute("CREATE TABLE dml_target_rows(id integer PRIMARY KEY, value integer)")
            reader.execute("INSERT INTO dml_target_rows VALUES (1, 10), (2, 20)")
            returned = "OLD" if "DELETE" in event else "NEW"
            reader.execute(f"CREATE FUNCTION dml_target_action() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF OLD.id = 1 THEN {mutation}; END IF; RETURN {returned}; END $$")
            reader.execute(f"CREATE TRIGGER dml_target_trigger {event} ON dml_target_rows FOR EACH ROW EXECUTE FUNCTION dml_target_action()")
            error = sqlstate(reader, command)
            rows = reader.execute("SELECT id, value FROM dml_target_rows ORDER BY id").fetchall()
            results.append(f"dml_target_{label}={error},rows:{rows}")
            reader.execute("DROP TABLE dml_target_rows")
            reader.execute("DROP FUNCTION dml_target_action()")
        reader.execute("CREATE TABLE dml_target_rows(id integer PRIMARY KEY, value integer)")
        reader.execute("INSERT INTO dml_target_rows VALUES (1, 10), (2, 20), (3, 30)")
        changed = reader.execute("UPDATE dml_target_rows AS target SET value = source.value FROM dml_target_rows AS source WHERE source.id = target.id - 1 RETURNING target.id, target.value").fetchall()
        rows = reader.execute("SELECT id, value FROM dml_target_rows ORDER BY id").fetchall()
        results.append(f"dml_target_self_join=changed:{changed},rows:{rows}")
        reader.execute("DROP TABLE dml_target_rows")
        reader.execute("CREATE TABLE dml_target_rows(id integer PRIMARY KEY, value integer)")
        reader.execute("INSERT INTO dml_target_rows VALUES (1, 10), (2, 20)")
        changed = reader.execute("UPDATE dml_target_rows AS target SET value = value + 1 RETURNING id, value, (SELECT sum(value) FROM dml_target_rows), (SELECT target.value), (SELECT old.value), (SELECT new.value), ARRAY(SELECT id FROM dml_target_rows ORDER BY id)").fetchall()
        results.append(f"dml_target_update_returning_snapshot={sorted(changed)}")
        deleted = reader.execute("DELETE FROM dml_target_rows AS target RETURNING id, value, (SELECT sum(value) FROM dml_target_rows), (SELECT target.value), (SELECT old.value), (SELECT new.value), ARRAY(SELECT id FROM dml_target_rows ORDER BY id)").fetchall()
        results.append(f"dml_target_delete_returning_snapshot={sorted(deleted)}")
        empty = reader.execute("UPDATE dml_target_rows SET value = value RETURNING (SELECT sum(value) FROM dml_target_rows), ARRAY(SELECT id FROM dml_target_rows)")
        types = [column.type_code for column in empty.description]
        results.append(f"dml_target_empty_returning=rows:{empty.fetchall()},types:{types}")
        inserted = reader.execute("INSERT INTO dml_target_rows VALUES (3, 30) RETURNING id, value, (SELECT count(*) FROM dml_target_rows), (SELECT old.value), (SELECT new.value)").fetchall()
        results.append(f"dml_target_insert_returning_snapshot={inserted}")
        conflicted = reader.execute("INSERT INTO dml_target_rows VALUES (3, 40) ON CONFLICT (id) DO UPDATE SET value = excluded.value RETURNING id, value, (SELECT sum(value) FROM dml_target_rows), (SELECT old.value), (SELECT new.value)").fetchall()
        results.append(f"dml_target_conflict_returning_snapshot={conflicted}")
        reader.execute("DROP TABLE dml_target_rows")
        reader.execute("CREATE TABLE dml_target_rows(id integer PRIMARY KEY, value integer)")
        reader.execute("CREATE FUNCTION dml_target_action() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.value < 100 THEN INSERT INTO dml_target_rows VALUES (NEW.id, 101); END IF; RETURN NEW; END $$")
        reader.execute("CREATE TRIGGER dml_target_trigger BEFORE INSERT ON dml_target_rows FOR EACH ROW EXECUTE FUNCTION dml_target_action()")
        for label, clause in [
            ("update", "DO UPDATE SET value = excluded.value"),
            ("filtered", "DO UPDATE SET value = excluded.value WHERE false"),
            ("nothing", "DO NOTHING"),
        ]:
            reader.execute("TRUNCATE dml_target_rows")
            error = sqlstate(reader, f"INSERT INTO dml_target_rows VALUES (1, 1) ON CONFLICT (id) {clause}")
            rows = reader.execute("SELECT id, value FROM dml_target_rows").fetchall()
            results.append(f"dml_target_trigger_conflict_{label}={error},rows:{rows}")
        return results
    finally:
        reader.execute("ROLLBACK")
        reader.execute("DROP TABLE IF EXISTS dml_target_rows CASCADE")
        reader.execute("DROP FUNCTION IF EXISTS dml_target_action() CASCADE")


def dml_description_transcript(reader, writer) -> list[str]:
    writer.execute("CREATE TABLE described_orders_diff(id integer PRIMARY KEY, amount bigint, label text)")
    writer.execute("INSERT INTO described_orders_diff VALUES (1, 10, 'first')")
    writer.execute("CREATE VIEW described_view_diff(order_id, total, note) AS SELECT id, amount, label FROM described_orders_diff")
    results = []
    try:
        cases = [
            ("update", "WITH changed AS (UPDATE described_view_diff SET total = total + 2 WHERE order_id = 1 RETURNING order_id, total, note) SELECT order_id, total, note FROM changed"),
            ("insert", "WITH changed AS (INSERT INTO described_view_diff VALUES (2, 20, 'second') RETURNING order_id, total, note) SELECT order_id, total, note FROM changed"),
            ("delete", "WITH changed AS (DELETE FROM described_view_diff WHERE order_id = 1 RETURNING order_id, total, note) SELECT order_id, total, note FROM changed"),
            ("empty", "WITH changed AS (UPDATE described_view_diff SET total = total WHERE false RETURNING order_id, total, note) SELECT order_id, total, note FROM changed"),
            ("renamed", "WITH changed(order_number, spent, message) AS (UPDATE described_view_diff SET total = total WHERE order_id = 1 RETURNING *) SELECT order_number, spent, message FROM changed"),
            ("prepared", "UPDATE described_view_diff SET total = total WHERE order_id = 1 RETURNING order_id, total, note"),
        ]
        for label, statement in cases:
            writer.execute("TRUNCATE described_orders_diff")
            writer.execute("INSERT INTO described_orders_diff VALUES (1, 10, 'first')")
            try:
                cursor = reader.execute(statement, prepare=True)
                names = [column.name for column in cursor.description]
                types = [column.type_code for column in cursor.description]
                rows = cursor.fetchall()
                results.append(f"dml_description_{label}=names:{names},types:{types},rows:{rows}")
            except psycopg.Error as error:
                results.append(f"dml_description_{label}=error:{error.sqlstate}")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("DROP TABLE described_orders_diff CASCADE")


def index_validation_transcript(reader, writer) -> list[str]:
    writer.execute("DROP TABLE IF EXISTS index_validation_diff")
    writer.execute("CREATE TABLE index_validation_diff (id integer, value integer)")
    writer.execute("INSERT INTO index_validation_diff VALUES (1, 10), (2, 20)")
    results = []
    try:
        for change, expected_old in [
            ("UPDATE index_validation_diff SET value = 10 WHERE id = 2", 20),
            ("INSERT INTO index_validation_diff VALUES (2, 10)", None),
        ]:
            reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
            before = reader.execute(
                "SELECT value FROM index_validation_diff WHERE id = 2"
            ).fetchone()
            assert before == (None if expected_old is None else (expected_old,)), before
            writer.execute(change)
            reader.execute("SAVEPOINT before_validation")
            result = sqlstate(
                reader,
                "CREATE UNIQUE INDEX index_validation_key ON index_validation_diff(value)",
            )
            reader.execute("ROLLBACK TO SAVEPOINT before_validation")
            after = reader.execute(
                "SELECT value FROM index_validation_diff WHERE id = 2"
            ).fetchone()
            results.append(
                f"index_new_commits={result},snapshot_unchanged={before == after}"
            )
            reader.execute("ROLLBACK")
            writer.execute("DELETE FROM index_validation_diff WHERE id = 2")
        writer.execute("INSERT INTO index_validation_diff VALUES (2, 20)")
        reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        before = reader.execute("SELECT value FROM index_validation_diff WHERE id = 2").fetchone()
        writer.execute("UPDATE index_validation_diff SET value = 30 WHERE id = 2")
        reader.execute("CREATE UNIQUE INDEX index_validation_key ON index_validation_diff(value)")
        after = reader.execute("SELECT value FROM index_validation_diff WHERE id = 2").fetchone()
        results.append(f"index_valid_current=ok,snapshot_unchanged={before == after}")
        reader.execute("ROLLBACK")
        writer.execute("CREATE UNIQUE INDEX index_validation_key ON index_validation_diff(value)")
        enforced = sqlstate(writer, "INSERT INTO index_validation_diff VALUES (3, 30)")
        results.append(f"index_enforces_current={enforced}")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("DROP TABLE index_validation_diff")


def partition_index_validation_transcript(reader, writer) -> list[str]:
    writer.execute("DROP TABLE IF EXISTS partition_index_diff CASCADE")
    writer.execute("CREATE TABLE partition_index_diff (bucket integer, value integer) PARTITION BY RANGE (bucket)")
    writer.execute("CREATE TABLE partition_index_low PARTITION OF partition_index_diff FOR VALUES FROM (0) TO (10)")
    writer.execute("CREATE TABLE partition_index_high PARTITION OF partition_index_diff FOR VALUES FROM (10) TO (20)")
    results = []
    try:
        for label, change in [
            ("update", "UPDATE partition_index_diff SET value = 10 WHERE value = 20"),
            ("insert", "INSERT INTO partition_index_diff VALUES (1, 10)"),
        ]:
            writer.execute("TRUNCATE partition_index_diff")
            writer.execute("INSERT INTO partition_index_diff VALUES (1, 10), (1, 20), (11, 30)")
            reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
            before = reader.execute("SELECT value FROM partition_index_diff WHERE bucket = 1 ORDER BY value").fetchall()
            writer.execute(change)
            reader.execute("SAVEPOINT before_partition_index")
            error = sqlstate(reader, "CREATE UNIQUE INDEX partition_index_key ON partition_index_diff(bucket, value)")
            reader.execute("ROLLBACK TO SAVEPOINT before_partition_index")
            after = reader.execute("SELECT value FROM partition_index_diff WHERE bucket = 1 ORDER BY value").fetchall()
            results.append(f"partition_index_{label}={error},before:{before},after:{after}")
            reader.execute("ROLLBACK")
        writer.execute("TRUNCATE partition_index_diff")
        writer.execute("INSERT INTO partition_index_diff VALUES (1, 10), (1, 20), (11, 30)")
        reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        before = reader.execute("SELECT value FROM partition_index_diff WHERE bucket = 1 ORDER BY value").fetchall()
        writer.execute("UPDATE partition_index_diff SET value = 30 WHERE value = 20")
        reader.execute("CREATE UNIQUE INDEX partition_index_key ON partition_index_diff(bucket, value)")
        after = reader.execute("SELECT value FROM partition_index_diff WHERE bucket = 1 ORDER BY value").fetchall()
        reader.execute("ROLLBACK")
        remaining = writer.execute("SELECT count(*) FROM pg_indexes WHERE tablename IN ('partition_index_diff', 'partition_index_low', 'partition_index_high')").fetchone()[0]
        results.append(f"partition_index_valid=before:{before},after:{after},rolled_back:{remaining}")
        writer.execute("CREATE UNIQUE INDEX partition_index_key ON partition_index_diff(bucket, value)")
        error = sqlstate(writer, "INSERT INTO partition_index_diff VALUES (1, 30)")
        results.append(f"partition_index_enforced={error}")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("ROLLBACK")
        writer.execute("DROP TABLE partition_index_diff CASCADE")


def constraint_validation_transcript(reader, writer) -> list[str]:
    results = []
    cases = [
        (
            "check", "CREATE TABLE constraint_current(id integer, value integer)",
            "INSERT INTO constraint_current VALUES (3, -1)",
            "ALTER TABLE constraint_current ADD CONSTRAINT positive CHECK (value > 0)",
        ),
        (
            "not_null", "CREATE TABLE constraint_current(id integer, value integer)",
            "INSERT INTO constraint_current VALUES (3, NULL)",
            "ALTER TABLE constraint_current ALTER COLUMN value SET NOT NULL",
        ),
        (
            "unique", "CREATE TABLE constraint_current(id integer, value integer)",
            "INSERT INTO constraint_current VALUES (3, 30), (4, 30)",
            "ALTER TABLE constraint_current ADD CONSTRAINT distinct_value UNIQUE (value)",
        ),
        (
            "foreign_key", "CREATE TABLE constraint_current(id integer, value integer)",
            "INSERT INTO constraint_current VALUES (3, 99)",
            "ALTER TABLE constraint_current ADD CONSTRAINT known_value FOREIGN KEY (value) REFERENCES constraint_parent(id)",
        ),
    ]
    writer.execute("DROP TABLE IF EXISTS constraint_current CASCADE")
    writer.execute("DROP TABLE IF EXISTS constraint_parent CASCADE")
    writer.execute("CREATE TABLE constraint_parent(id integer PRIMARY KEY)")
    writer.execute("INSERT INTO constraint_parent VALUES (10), (20)")
    try:
        for label, setup, change, command in cases:
            writer.execute(setup)
            writer.execute("INSERT INTO constraint_current VALUES (1, 10), (2, 20)")
            reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
            before = reader.execute("SELECT count(*) FROM constraint_current").fetchone()[0]
            writer.execute(change)
            reader.execute("SAVEPOINT before_validation")
            error = sqlstate(reader, command)
            reader.execute("ROLLBACK TO SAVEPOINT before_validation")
            after = reader.execute("SELECT count(*) FROM constraint_current").fetchone()[0]
            results.append(f"constraint_{label}={error},before:{before},after:{after}")
            reader.execute("ROLLBACK")
            writer.execute("DROP TABLE constraint_current CASCADE")
        for label, change, definition in [
            ("validate_check", "INSERT INTO constraint_current VALUES (3, -1)", "CHECK (value > 0)"),
            ("validate_fk", "INSERT INTO constraint_current VALUES (3, 99)", "FOREIGN KEY (value) REFERENCES constraint_parent(id)"),
        ]:
            writer.execute("CREATE TABLE constraint_current(id integer, value integer)")
            writer.execute("INSERT INTO constraint_current VALUES (1, 10), (2, 20)")
            reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
            before = reader.execute("SELECT count(*) FROM constraint_current").fetchone()[0]
            writer.execute(change)
            reader.execute(f"ALTER TABLE constraint_current ADD CONSTRAINT pending_validation {definition} NOT VALID")
            reader.execute("SAVEPOINT before_validation")
            error = sqlstate(reader, "ALTER TABLE constraint_current VALIDATE CONSTRAINT pending_validation")
            reader.execute("ROLLBACK TO SAVEPOINT before_validation")
            after = reader.execute("SELECT count(*) FROM constraint_current").fetchone()[0]
            valid = reader.execute("SELECT convalidated FROM pg_constraint WHERE conname = 'pending_validation'").fetchone()[0]
            results.append(f"constraint_{label}={error},before:{before},after:{after},valid:{valid}")
            reader.execute("ROLLBACK")
            writer.execute("DROP TABLE constraint_current CASCADE")
        writer.execute("CREATE TABLE constraint_current(id integer, value integer)")
        writer.execute("INSERT INTO constraint_current VALUES (1, 10), (2, 20)")
        reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        before = reader.execute("SELECT count(*) FROM constraint_current").fetchone()[0]
        writer.execute("INSERT INTO constraint_parent VALUES (30)")
        writer.execute("INSERT INTO constraint_current VALUES (3, 30)")
        reader.execute("SAVEPOINT before_validation")
        error = sqlstate(reader, "ALTER TABLE constraint_current ADD CONSTRAINT current_parent FOREIGN KEY (value) REFERENCES constraint_parent(id)")
        reader.execute("ROLLBACK TO SAVEPOINT before_validation")
        after = reader.execute("SELECT count(*) FROM constraint_current").fetchone()[0]
        results.append(f"constraint_current_parent={error},before:{before},after:{after}")
        reader.execute("ROLLBACK")
        writer.execute("DROP TABLE constraint_current")
        for label, change, command in [
            ("domain_check", "INSERT INTO constraint_current VALUES (3, -1)", "ALTER DOMAIN constraint_domain ADD CONSTRAINT domain_positive CHECK (VALUE > 0)"),
            ("domain_not_null", "INSERT INTO constraint_current VALUES (3, NULL)", "ALTER DOMAIN constraint_domain SET NOT NULL"),
        ]:
            writer.execute("CREATE DOMAIN constraint_domain AS integer")
            writer.execute("CREATE TABLE constraint_current(id integer, value constraint_domain)")
            writer.execute("INSERT INTO constraint_current VALUES (1, 10), (2, 20)")
            reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
            before = reader.execute("SELECT count(*) FROM constraint_current").fetchone()[0]
            writer.execute(change)
            reader.execute("SAVEPOINT before_validation")
            error = sqlstate(reader, command)
            reader.execute("ROLLBACK TO SAVEPOINT before_validation")
            after = reader.execute("SELECT count(*) FROM constraint_current").fetchone()[0]
            results.append(f"constraint_{label}={error},before:{before},after:{after}")
            reader.execute("ROLLBACK")
            writer.execute("DROP TABLE constraint_current")
            writer.execute("DROP DOMAIN constraint_domain")
        writer.execute("CREATE TABLE constraint_partition(id integer) PARTITION BY RANGE (id)")
        writer.execute("CREATE TABLE constraint_current(id integer)")
        writer.execute("INSERT INTO constraint_current VALUES (1)")
        reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        before = reader.execute("SELECT count(*) FROM constraint_current").fetchone()[0]
        writer.execute("INSERT INTO constraint_current VALUES (99)")
        reader.execute("SAVEPOINT before_validation")
        error = sqlstate(reader, "ALTER TABLE constraint_partition ATTACH PARTITION constraint_current FOR VALUES FROM (0) TO (10)")
        reader.execute("ROLLBACK TO SAVEPOINT before_validation")
        after = reader.execute("SELECT count(*) FROM constraint_current").fetchone()[0]
        results.append(f"constraint_partition={error},before:{before},after:{after}")
        reader.execute("ROLLBACK")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("ROLLBACK")
        writer.execute("DROP TABLE IF EXISTS constraint_current CASCADE")
        writer.execute("DROP TABLE IF EXISTS constraint_partition CASCADE")
        writer.execute("DROP TABLE IF EXISTS constraint_parent CASCADE")
        writer.execute("DROP DOMAIN IF EXISTS constraint_domain CASCADE")


def constraint_partition_transcript(reader, writer) -> list[str]:
    results = []
    try:
        writer.execute("CREATE TABLE validation_fk_parent(id integer PRIMARY KEY) PARTITION BY RANGE (id)")
        writer.execute("CREATE TABLE validation_fk_leaf PARTITION OF validation_fk_parent FOR VALUES FROM (0) TO (10)")
        writer.execute("INSERT INTO validation_fk_parent VALUES (1)")
        writer.execute("CREATE TABLE validation_fk_child(id integer, parent_id integer)")
        writer.execute("INSERT INTO validation_fk_child VALUES (1, 1)")
        reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        before = reader.execute("SELECT count(*) FROM validation_fk_child").fetchone()[0]
        writer.execute("INSERT INTO validation_fk_parent VALUES (2)")
        writer.execute("INSERT INTO validation_fk_child VALUES (2, 2)")
        reader.execute("SAVEPOINT before_validation")
        command = "ALTER TABLE validation_fk_child ADD CONSTRAINT partition_parent FOREIGN KEY (parent_id) REFERENCES validation_fk_parent(id)"
        error = sqlstate(reader, command)
        reader.execute("ROLLBACK TO SAVEPOINT before_validation")
        after = reader.execute("SELECT count(*) FROM validation_fk_child").fetchone()[0]
        results.append(f"constraint_partition_parent={error},before:{before},after:{after}")
        reader.execute("ROLLBACK")
        results.append(f"constraint_partition_fk_created={sqlstate(writer, command)}")
        for label, command in [
            ("valid_insert", "INSERT INTO validation_fk_child VALUES (3, 2)"),
            ("invalid_insert", "INSERT INTO validation_fk_child VALUES (3, 99)"),
            ("root_delete", "DELETE FROM validation_fk_parent WHERE id = 2"),
            ("leaf_delete", "DELETE FROM validation_fk_leaf WHERE id = 2"),
        ]:
            writer.execute("BEGIN")
            error = sqlstate(writer, command)
            writer.execute("ROLLBACK")
            results.append(f"constraint_partition_fk_{label}={error}")
        writer.execute("DROP TABLE validation_fk_child")
        writer.execute("DROP TABLE validation_fk_parent CASCADE")
        for label, nested, attaching in [("create", False, False), ("attach", False, True), ("nested_attach", True, True)]:
            writer.execute("CREATE TABLE validation_default_root(id integer) PARTITION BY RANGE (id)")
            suffix = " PARTITION BY RANGE (id)" if nested else ""
            writer.execute(f"CREATE TABLE validation_default PARTITION OF validation_default_root DEFAULT{suffix}")
            if nested:
                writer.execute("CREATE TABLE validation_default_leaf PARTITION OF validation_default FOR VALUES FROM (MINVALUE) TO (MAXVALUE)")
            if attaching:
                writer.execute("CREATE TABLE validation_new_partition(id integer)")
            writer.execute("INSERT INTO validation_default_root VALUES (99)")
            reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
            before = reader.execute("SELECT count(*) FROM validation_default").fetchone()[0]
            writer.execute("INSERT INTO validation_default_root VALUES (5)")
            reader.execute("SAVEPOINT before_validation")
            command = (
                "ALTER TABLE validation_default_root ATTACH PARTITION validation_new_partition FOR VALUES FROM (0) TO (10)"
                if attaching else
                "CREATE TABLE validation_new_partition PARTITION OF validation_default_root FOR VALUES FROM (0) TO (10)"
            )
            error = sqlstate(reader, command)
            reader.execute("ROLLBACK TO SAVEPOINT before_validation")
            after = reader.execute("SELECT count(*) FROM validation_default").fetchone()[0]
            results.append(f"constraint_default_{label}={error},before:{before},after:{after}")
            reader.execute("ROLLBACK")
            writer.execute("DELETE FROM validation_default WHERE id = 5")
            corrected = sqlstate(writer, command)
            writer.execute("INSERT INTO validation_default_root VALUES (5)")
            default_rows = writer.execute("SELECT count(*) FROM validation_default").fetchone()[0]
            new_rows = writer.execute("SELECT count(*) FROM validation_new_partition").fetchone()[0]
            results.append(f"constraint_default_{label}_retry={corrected},default:{default_rows},new:{new_rows}")
            writer.execute("DROP TABLE validation_default_root CASCADE")
        writer.execute("CREATE TABLE validation_expression_lookup(value integer)")
        writer.execute("INSERT INTO validation_expression_lookup VALUES (100)")
        writer.execute("CREATE FUNCTION validation_expression_value() RETURNS integer LANGUAGE sql STABLE AS $$SELECT max(value) FROM validation_expression_lookup$$")
        writer.execute("CREATE TABLE validation_expression_rows(value integer)")
        writer.execute("INSERT INTO validation_expression_rows VALUES (10), (20)")
        reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        before = reader.execute("SELECT validation_expression_value()").fetchone()[0]
        writer.execute("UPDATE validation_expression_lookup SET value = 0")
        reader.execute("SAVEPOINT before_validation")
        error = sqlstate(reader, "ALTER TABLE validation_expression_rows ADD CONSTRAINT expression_rule CHECK (value < validation_expression_value())")
        reader.execute("ROLLBACK TO SAVEPOINT before_validation")
        after = reader.execute("SELECT validation_expression_value()").fetchone()[0]
        results.append(f"constraint_expression={error},before:{before},after:{after}")
        reader.execute("ROLLBACK")
        writer.execute("CREATE TABLE validation_exclusion(id integer, value int4range)")
        writer.execute("INSERT INTO validation_exclusion VALUES (1, '[1,5)'), (2, '[10,20)')")
        reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        before = reader.execute("SELECT count(*) FROM validation_exclusion").fetchone()[0]
        writer.execute("INSERT INTO validation_exclusion VALUES (3, '[30,40)'), (4, '[35,45)')")
        reader.execute("SAVEPOINT before_validation")
        error = sqlstate(reader, "ALTER TABLE validation_exclusion ADD CONSTRAINT exclusion_rule EXCLUDE USING gist (value WITH &&)")
        reader.execute("ROLLBACK TO SAVEPOINT before_validation")
        after = reader.execute("SELECT count(*) FROM validation_exclusion").fetchone()[0]
        results.append(f"constraint_exclusion={error},before:{before},after:{after}")
        reader.execute("ROLLBACK")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("ROLLBACK")
        for relation in ("validation_fk_child", "validation_fk_parent", "validation_default_root", "validation_new_partition", "validation_expression_rows", "validation_expression_lookup", "validation_exclusion"):
            writer.execute(f"DROP TABLE IF EXISTS {relation} CASCADE")
        writer.execute("DROP FUNCTION IF EXISTS validation_expression_value()")


def constraint_rewrite_transcript(reader, writer) -> list[str]:
    writer.execute("CREATE TABLE validation_rewrite(id integer, value text)")
    writer.execute("INSERT INTO validation_rewrite VALUES (1, '10'), (2, '20')")
    try:
        reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        before = reader.execute("SELECT count(*) FROM validation_rewrite").fetchone()[0]
        writer.execute("INSERT INTO validation_rewrite VALUES (3, 'invalid')")
        reader.execute("SAVEPOINT before_rewrite")
        command = "ALTER TABLE validation_rewrite ALTER COLUMN value TYPE bigint USING value::bigint"
        error = sqlstate(reader, command)
        reader.execute("ROLLBACK TO SAVEPOINT before_rewrite")
        unchanged = reader.execute("SELECT count(*) FROM validation_rewrite").fetchone()[0]
        writer.execute("UPDATE validation_rewrite SET value = '30' WHERE id = 3")
        retried = sqlstate(reader, command)
        if retried != "ok":
            reader.execute("ROLLBACK TO SAVEPOINT before_rewrite")
        converted = reader.execute("SELECT id, value FROM validation_rewrite ORDER BY id").fetchall()
        reader.execute("ROLLBACK")
        restored = writer.execute("SELECT id, value FROM validation_rewrite ORDER BY id").fetchall()
        return [
            f"constraint_rewrite_error={error},before:{before},after:{unchanged}",
            f"constraint_rewrite_retry={retried},converted:{converted},restored:{restored}",
        ]
    finally:
        reader.execute("ROLLBACK")
        writer.execute("ROLLBACK")
        writer.execute("DROP TABLE validation_rewrite")


def analyze_snapshot_transcript(reader, writer) -> list[str]:
    writer.execute("DROP TABLE IF EXISTS analyze_snapshot_diff CASCADE")
    writer.execute("CREATE TABLE analyze_snapshot_diff (id integer, a integer, b integer, label text)")
    writer.execute("INSERT INTO analyze_snapshot_diff VALUES (1, 1, 1, 'old'), (2, 1, 1, 'old')")
    writer.execute("CREATE STATISTICS analyze_snapshot_ab (ndistinct, dependencies, mcv) ON a, b FROM analyze_snapshot_diff")
    writer.execute("ANALYZE analyze_snapshot_diff")
    results = []

    def report(stage):
        count = reader.execute("SELECT count(*) FROM analyze_snapshot_diff").fetchone()[0]
        estimated = reader.execute("SELECT reltuples::integer FROM pg_class WHERE oid = 'analyze_snapshot_diff'::regclass").fetchone()[0]
        nulls = reader.execute("SELECT null_frac > 0.3 AND null_frac < 0.4 FROM pg_stats WHERE tablename = 'analyze_snapshot_diff' AND attname = 'label'").fetchone()[0]
        frequent = reader.execute("SELECT most_common_freqs[1] > 0.6 AND most_common_freqs[1] < 0.7 FROM pg_stats_ext WHERE statistics_name = 'analyze_snapshot_ab'").fetchone()[0]
        results.append(f"analyze_{stage}=rows:{estimated},query:{count},nulls:{nulls},mcv:{frequent}")

    try:
        reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        assert reader.execute("SELECT count(*) FROM analyze_snapshot_diff").fetchone()[0] == 2
        writer.execute("INSERT INTO analyze_snapshot_diff VALUES (3, 2, 2, NULL)")
        reader.execute("ANALYZE analyze_snapshot_diff")
        report("committed")
        writer.execute("BEGIN")
        writer.execute("UPDATE analyze_snapshot_diff SET a = 9, label = NULL WHERE id = 1")
        writer.execute("DELETE FROM analyze_snapshot_diff WHERE id = 2")
        writer.execute("INSERT INTO analyze_snapshot_diff VALUES (4, 9, 9, NULL)")
        reader.execute("ANALYZE analyze_snapshot_diff")
        report("foreign_pending")
        writer.execute("ROLLBACK")
        reader.execute("SAVEPOINT before_changes")
        reader.execute("UPDATE analyze_snapshot_diff SET a = 4, b = 4, label = NULL WHERE id = 1")
        reader.execute("DELETE FROM analyze_snapshot_diff WHERE id = 2")
        reader.execute("INSERT INTO analyze_snapshot_diff VALUES (4, 4, 4, NULL)")
        reader.execute("ANALYZE analyze_snapshot_diff")
        own_nulls = reader.execute("SELECT null_frac = 1 FROM pg_stats WHERE tablename = 'analyze_snapshot_diff' AND attname = 'label'").fetchone()[0]
        own_count = reader.execute("SELECT count(*) FROM analyze_snapshot_diff").fetchone()[0]
        results.append(f"analyze_own=all_null:{own_nulls},query:{own_count}")
        reader.execute("ROLLBACK TO SAVEPOINT before_changes")
        report("rolled_back")
        reader.execute("CREATE STATISTICS analyze_snapshot_error ON (100 / (id - 3)) FROM analyze_snapshot_diff")
        reader.execute("SAVEPOINT before_error")
        error = sqlstate(reader, "ANALYZE analyze_snapshot_diff")
        reader.execute("ROLLBACK TO SAVEPOINT before_error")
        unchanged = reader.execute("SELECT count(*) FROM analyze_snapshot_diff").fetchone()[0]
        results.append(f"analyze_error={error},query:{unchanged}")
        reader.execute("DROP STATISTICS analyze_snapshot_error")
        reader.execute("ANALYZE analyze_snapshot_diff")
        report("retried")
        reader.execute("ROLLBACK")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("ROLLBACK")
        writer.execute("DROP TABLE analyze_snapshot_diff CASCADE")


def exercise(port: int) -> list[str]:
    reader = connect(port)
    writer = connect(port)
    try:
        writer.execute("DROP TABLE IF EXISTS mvcc_diff")
        writer.execute("CREATE TABLE mvcc_diff (id integer PRIMARY KEY, value text)")
        writer.execute("INSERT INTO mvcc_diff VALUES (1, 'old')")

        reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        first = reader.execute(
            "SELECT value FROM mvcc_diff WHERE id = 1"
        ).fetchone()[0]
        writer.execute("UPDATE mvcc_diff SET value = 'new' WHERE id = 1")
        second = reader.execute(
            "SELECT value FROM mvcc_diff WHERE id = 1"
        ).fetchone()[0]
        read_only_error = sqlstate(
            reader, "INSERT INTO mvcc_diff VALUES (2, 'forbidden')"
        )
        reader.execute("ROLLBACK")
        after = reader.execute(
            "SELECT value FROM mvcc_diff WHERE id = 1"
        ).fetchone()[0]

        reader.execute("BEGIN")
        reader.execute("LOCK TABLE mvcc_diff IN ACCESS SHARE MODE")
        writer.execute("SET lock_timeout = '50ms'")
        lock_error = sqlstate(
            writer, "ALTER TABLE mvcc_diff ADD COLUMN blocked integer"
        )
        reader.execute("ROLLBACK")
        writer.execute("ALTER TABLE mvcc_diff ADD COLUMN allowed integer")

        return [
            f"snapshot={first},{second},{after}",
            f"read_only={read_only_error}",
            f"lock={lock_error}",
            "ddl_after_unlock=ok",
        ] + dml_target_transcript(reader, writer) + dml_description_transcript(reader, writer) + on_conflict_snapshot_transcript(reader, writer) + index_validation_transcript(reader, writer) + partition_index_validation_transcript(reader, writer) + constraint_validation_transcript(reader, writer) + constraint_partition_transcript(reader, writer) + constraint_rewrite_transcript(reader, writer) + analyze_snapshot_transcript(reader, writer)
    finally:
        reader.close()
        writer.close()


def main() -> int:
    postgres_port = int(os.environ.get("PGPORT", "5432"))
    pos3ql_port = int(os.environ.get("P3_PORT", "15599"))
    expected = exercise(postgres_port)
    observed = exercise(pos3ql_port)
    if observed != expected:
        print("MVCC divergence", file=sys.stderr)
        print(f"PostgreSQL: {expected}", file=sys.stderr)
        print(f"pos3ql:     {observed}", file=sys.stderr)
        return 1
    for line in observed:
        print(line)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
