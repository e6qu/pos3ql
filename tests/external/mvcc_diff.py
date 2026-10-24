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


def transaction_admission_transcript(reader, writer) -> list[str]:
    results = []
    writer.execute("CREATE TABLE admission_order_diff(id integer PRIMARY KEY, total integer)")
    writer.execute("CREATE TABLE admission_invoice_diff(id integer PRIMARY KEY, total integer)")
    query = "SELECT o.total,i.total FROM admission_order_diff o JOIN admission_invoice_diff i ON o.id=i.id"
    try:
        for isolation in ["READ COMMITTED", "REPEATABLE READ"]:
            writer.execute("TRUNCATE admission_order_diff,admission_invoice_diff")
            writer.execute("INSERT INTO admission_order_diff VALUES(1,10)")
            writer.execute("INSERT INTO admission_invoice_diff VALUES(1,10)")
            reader.execute(f"BEGIN ISOLATION LEVEL {isolation}")
            before = reader.execute(query).fetchall()
            writer.execute("BEGIN")
            writer.execute("UPDATE admission_order_diff SET total=20")
            partial = reader.execute(query).fetchall()
            writer.execute("UPDATE admission_invoice_diff SET total=20")
            writer.execute("COMMIT")
            committed = reader.execute(query).fetchall()
            reader.execute("COMMIT")
            fresh = reader.execute(query).fetchall()
            writer.execute("BEGIN")
            writer.execute("UPDATE admission_order_diff SET total=30")
            writer.execute("UPDATE admission_invoice_diff SET total=30")
            writer.execute("ROLLBACK")
            rolled_back = reader.execute(query).fetchall()
            results.append(f"transaction_admission_{isolation}=before:{before},partial:{partial},committed:{committed},fresh:{fresh},rollback:{rolled_back}")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("ROLLBACK")
        writer.execute("DROP TABLE admission_invoice_diff")
        writer.execute("DROP TABLE admission_order_diff")


def generated_write_transcript(reader, writer) -> list[str]:
    writer.execute("CREATE TABLE generated_write_diff(id integer PRIMARY KEY, seed integer DEFAULT 4, result integer GENERATED ALWAYS AS (100 / seed) STORED)")
    writer.execute("CREATE FUNCTION generated_write_before_diff() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN NEW.seed := 2; RETURN NEW; END$$")
    writer.execute("CREATE TRIGGER generated_write_before_diff BEFORE INSERT ON generated_write_diff FOR EACH ROW EXECUTE FUNCTION generated_write_before_diff()")
    writer.execute("CREATE TABLE generated_serial_diff(id serial PRIMARY KEY, label text UNIQUE)")
    results = []
    try:
        reader.execute("INSERT INTO generated_write_diff(id, seed) VALUES (1, 0)")
        reader.execute("MERGE INTO generated_write_diff t USING (VALUES (2, 0)) AS s(id, seed) ON t.id = s.id WHEN NOT MATCHED THEN INSERT(id, seed) VALUES (s.id, s.seed)")
        with reader.cursor().copy("COPY generated_write_diff(id, seed) FROM STDIN") as copied:
            copied.write(b"3\t0\n")
        with reader.cursor().copy("COPY generated_write_diff(id, seed) FROM STDIN (FORMAT binary)") as copied:
            copied.set_types(["int4", "int4"])
            copied.write_row((4, 0))
        reader.execute("DROP TRIGGER generated_write_before_diff ON generated_write_diff")
        with reader.cursor().copy("COPY generated_write_diff(id, seed) FROM STDIN WHERE id < 0") as copied:
            copied.write(b"5\t0\n")
        rows = reader.execute("SELECT id, seed, result FROM generated_write_diff ORDER BY id").fetchall()
        results.append(f"generated_after_before={rows}")
        changed = reader.execute("INSERT INTO generated_write_diff(id, seed) VALUES (1, 1) ON CONFLICT(id) DO UPDATE SET seed = DEFAULT, result = DEFAULT RETURNING id, seed, result").fetchall()
        results.append(f"upsert_default_generated={changed}")
        for label, statement in [
            ("generated_assignment", "INSERT INTO generated_write_diff(id, seed) VALUES (5, 1) ON CONFLICT(id) DO UPDATE SET result = 99"),
            ("merge_generated_assignment", "MERGE INTO generated_write_diff t USING (VALUES (-1)) AS s(id) ON t.id = s.id WHEN MATCHED THEN UPDATE SET result = 99"),
            ("generated_partition_key", "CREATE TABLE generated_key_diff(seed integer, result integer GENERATED ALWAYS AS (seed + 1) STORED) PARTITION BY RANGE(result)"),
        ]:
            error = sqlstate(reader, statement)
            rows = reader.execute("SELECT id, seed, result FROM generated_write_diff ORDER BY id").fetchall()
            results.append(f"{label}={error},rows:{rows}")
        reader.execute("INSERT INTO generated_serial_diff VALUES (10, 'kept')")
        changed = reader.execute("INSERT INTO generated_serial_diff(label) VALUES ('kept') ON CONFLICT(label) DO UPDATE SET id = DEFAULT RETURNING id, label").fetchall()
        results.append(f"upsert_default_serial={changed}")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("DROP TABLE generated_write_diff")
        writer.execute("DROP TABLE generated_serial_diff")
        writer.execute("DROP FUNCTION generated_write_before_diff()")


def partition_attachment_transcript(reader, writer) -> list[str]:
    writer.execute("CREATE TABLE attach_root_diff(id integer, group_id integer, label text) PARTITION BY RANGE(id)")
    writer.execute("CREATE TABLE attach_middle_diff(label text, id integer, group_id integer) PARTITION BY LIST(group_id)")
    writer.execute("CREATE TABLE attach_leaf_diff(group_id integer, label text, id integer)")
    writer.execute("CREATE TABLE attach_default_diff(label text, id integer, group_id integer)")
    writer.execute("CREATE TABLE attach_high_diff(group_id integer, id integer, label text)")
    writer.execute("INSERT INTO attach_leaf_diff VALUES (1, 'existing', 1)")
    writer.execute("INSERT INTO attach_default_diff VALUES ('default', 50, 9)")
    writer.execute("ALTER TABLE attach_middle_diff ATTACH PARTITION attach_leaf_diff FOR VALUES IN(1)")
    writer.execute("ALTER TABLE attach_root_diff ATTACH PARTITION attach_middle_diff FOR VALUES FROM (0) TO (10)")
    writer.execute("ALTER TABLE attach_root_diff ATTACH PARTITION attach_default_diff DEFAULT")
    results = []
    try:
        reader.execute("INSERT INTO attach_root_diff VALUES (2, 1, 'nested')")
        rows = reader.execute("SELECT id, group_id, label FROM attach_root_diff ORDER BY id").fetchall()
        selected = reader.execute("SELECT label FROM attach_root_diff WHERE id = 2 AND group_id = 1").fetchall()
        error = sqlstate(reader, "INSERT INTO attach_root_diff VALUES (3, 2, 'unmatched nested')")
        results.append(f"attach_nested=rows:{rows},selected:{selected},missing:{error}")
        error = sqlstate(reader, "ALTER TABLE attach_root_diff ATTACH PARTITION attach_high_diff FOR VALUES FROM (40) TO (60)")
        rows = reader.execute("SELECT id, group_id, label FROM attach_root_diff ORDER BY id").fetchall()
        results.append(f"attach_default_conflict={error},rows:{rows}")
        reader.execute("DELETE FROM attach_default_diff WHERE id = 50")
        reader.execute("ALTER TABLE attach_root_diff ATTACH PARTITION attach_high_diff FOR VALUES FROM (40) TO (60)")
        reader.execute("INSERT INTO attach_root_diff VALUES (45, 2, 'high')")
        reader.execute("ALTER TABLE attach_root_diff ADD CONSTRAINT attachment_label_check CHECK(label <> 'bad')")
        error = sqlstate(reader, "UPDATE attach_root_diff SET label = 'bad' WHERE id = 2")
        results.append(f"attach_inherited_check={error}")
        reader.execute("ALTER TABLE attach_root_diff ALTER COLUMN group_id SET NOT NULL")
        error = sqlstate(reader, "UPDATE attach_root_diff SET group_id = NULL WHERE id = 45")
        results.append(f"attach_inherited_not_null={error}")
        reader.execute("ALTER TABLE attach_root_diff ALTER COLUMN group_id DROP NOT NULL")
        reader.execute("ALTER TABLE attach_root_diff DROP CONSTRAINT attachment_label_check")
        reader.execute("ALTER TABLE attach_middle_diff DETACH PARTITION attach_leaf_diff CONCURRENTLY")
        error = sqlstate(reader, "INSERT INTO attach_leaf_diff VALUES (2, 'outside saved bound', 3)")
        remaining = reader.execute("SELECT id, group_id, label FROM attach_root_diff ORDER BY id").fetchall()
        results.append(f"attach_detached_bound={error},remaining:{remaining}")
        reader.execute("ALTER TABLE attach_middle_diff ATTACH PARTITION attach_leaf_diff FOR VALUES IN(1)")
        rows = reader.execute("SELECT id, group_id, label FROM attach_root_diff ORDER BY id").fetchall()
        results.append(f"attach_again={rows}")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("DROP TABLE attach_root_diff CASCADE")
        writer.execute("DROP TABLE IF EXISTS attach_middle_diff CASCADE")
        writer.execute("DROP TABLE IF EXISTS attach_leaf_diff")
        writer.execute("DROP TABLE IF EXISTS attach_high_diff")


def wide_partition_transcript(reader, writer) -> list[str]:
    columns = [f"c{column} integer" for column in range(80)]
    writer.execute(f"CREATE TABLE wide_partition_diff({','.join(columns)}) PARTITION BY RANGE(c0)")
    writer.execute(f"CREATE TABLE wide_leaf_diff({','.join(reversed(columns))})")
    writer.execute("ALTER TABLE wide_partition_diff ATTACH PARTITION wide_leaf_diff DEFAULT")
    try:
        reader.execute("ALTER TABLE wide_partition_diff ADD CONSTRAINT wide_positive CHECK(c79 > 0)")
        reader.execute("INSERT INTO wide_partition_diff(c0, c79) VALUES (1, 2)")
        error = sqlstate(reader, "UPDATE wide_partition_diff SET c79 = -1")
        rows = reader.execute("SELECT c0, c79 FROM wide_partition_diff").fetchall()
        results = [f"wide_partition_check={error},rows:{rows}"]
        reader.execute("ALTER TABLE wide_partition_diff DROP CONSTRAINT wide_positive")
        reader.execute("UPDATE wide_partition_diff SET c79 = -1")
        rows = reader.execute("SELECT c0, c79 FROM wide_partition_diff").fetchall()
        return results + [f"wide_partition_drop={rows}"]
    finally:
        reader.execute("ROLLBACK")
        writer.execute("DROP TABLE wide_partition_diff CASCADE")


def partition_move_transcript(reader, writer, reordered=False) -> list[str]:
    writer.execute("CREATE TABLE move_write_diff(id integer PRIMARY KEY, label text, computed integer GENERATED ALWAYS AS (length(label)) STORED) PARTITION BY RANGE(id)")
    if reordered:
        writer.execute("CREATE TABLE move_low_diff(computed integer GENERATED ALWAYS AS (length(label) + 100) STORED, label text, id integer NOT NULL)")
        writer.execute("CREATE TABLE move_high_diff(label text, id integer NOT NULL, computed integer GENERATED ALWAYS AS (length(label) + 200) STORED)")
        writer.execute("ALTER TABLE move_write_diff ATTACH PARTITION move_low_diff FOR VALUES FROM (0) TO (10)")
        writer.execute("ALTER TABLE move_write_diff ATTACH PARTITION move_high_diff FOR VALUES FROM (10) TO (20)")
    else:
        writer.execute("CREATE TABLE move_low_diff PARTITION OF move_write_diff FOR VALUES FROM (0) TO (10)")
        writer.execute("CREATE TABLE move_high_diff PARTITION OF move_write_diff FOR VALUES FROM (10) TO (20)")
        writer.execute("ALTER TABLE move_low_diff ALTER COLUMN computed SET EXPRESSION AS (length(label) + 100)")
        writer.execute("ALTER TABLE move_high_diff ALTER COLUMN computed SET EXPRESSION AS (length(label) + 200)")
    writer.execute("ALTER TABLE move_write_diff ALTER COLUMN label SET DEFAULT 'parent default'")
    writer.execute("ALTER TABLE move_low_diff ALTER COLUMN label SET DEFAULT 'child default'")
    writer.execute("CREATE TABLE move_audit_diff(event text, leaf text, id integer, label text, computed integer)")
    writer.execute("""CREATE FUNCTION move_before_diff() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN
        IF TG_OP = 'DELETE' THEN
            IF OLD.label = 'SKIP_DELETE' THEN RETURN NULL; END IF;
            RETURN OLD;
        ELSIF TG_OP = 'UPDATE' THEN
            NEW.label := NEW.label || ':U';
            IF NEW.label = 'trigger_move:U' THEN NEW.id := 14; END IF;
        ELSE
            IF NEW.label = 'skip_insert:U' THEN RETURN NULL; END IF;
            NEW.label := upper(NEW.label);
        END IF;
        RETURN NEW;
    END$$""")
    writer.execute("""CREATE FUNCTION move_after_diff() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN
        IF TG_OP = 'DELETE' THEN INSERT INTO move_audit_diff VALUES (TG_OP, TG_TABLE_NAME, OLD.id, OLD.label, OLD.computed);
        ELSE INSERT INTO move_audit_diff VALUES (TG_OP, TG_TABLE_NAME, NEW.id, NEW.label, NEW.computed); END IF;
        RETURN NULL;
    END$$""")
    writer.execute("CREATE TRIGGER move_before_diff BEFORE INSERT OR UPDATE OR DELETE ON move_write_diff FOR EACH ROW EXECUTE FUNCTION move_before_diff()")
    writer.execute("CREATE TRIGGER move_after_diff AFTER INSERT OR UPDATE OR DELETE ON move_write_diff FOR EACH ROW EXECUTE FUNCTION move_after_diff()")
    writer.execute("INSERT INTO move_write_diff(id, label) VALUES (1, 'first'), (2, 'skip_delete'), (3, 'skip_insert')")
    writer.execute("TRUNCATE move_audit_diff")
    results = []
    try:
        with reader.cursor().copy("COPY move_write_diff(id, label) FROM STDIN") as copied:
            copied.write(b"4\tcopy text\n")
        with reader.cursor().copy("COPY move_write_diff(id, label) FROM STDIN (FORMAT binary)") as copied:
            copied.set_types(["int4", "text"])
            copied.write_row((5, "copy binary"))
        reader.execute("INSERT INTO move_write_diff(id) VALUES (6)")
        reader.execute("INSERT INTO move_low_diff(id) VALUES (7)")
        changed = reader.execute("INSERT INTO move_write_diff(id, label) VALUES (1, 'upsert') ON CONFLICT(id) DO UPDATE SET label = excluded.label RETURNING id, label, computed").fetchall()
        results.append(f"leaf_upsert={changed}")
        changed = reader.execute("INSERT INTO move_write_diff(id, label) SELECT 8, 'selected' RETURNING id, label, computed").fetchall()
        results.append(f"leaf_insert_select={changed}")
        changed = reader.execute("INSERT INTO move_write_diff(id, label) SELECT 8, 'selected conflict' ON CONFLICT(id) DO UPDATE SET label = excluded.label RETURNING id, label, computed").fetchall()
        results.append(f"leaf_select_conflict={changed}")
        for label, command in [
            ("update_move", "UPDATE move_write_diff SET id = 11, label = 'moved' WHERE id = 1 RETURNING id, label, computed"),
            ("merge_skip_delete", "MERGE INTO move_write_diff t USING (VALUES (2, 12)) AS s(old_id, new_id) ON t.id = s.old_id WHEN MATCHED THEN UPDATE SET id = s.new_id RETURNING t.id, t.label, t.computed"),
            ("merge_skip_insert", "MERGE INTO move_write_diff t USING (VALUES (3, 13)) AS s(old_id, new_id) ON t.id = s.old_id WHEN MATCHED THEN UPDATE SET id = s.new_id, label = 'skip_insert' RETURNING t.id, t.label, t.computed"),
            ("same_leaf", "UPDATE move_write_diff SET label = 'same' WHERE id = 11 RETURNING id, label, computed"),
            ("before_update_chosen_leaf", "UPDATE move_write_diff SET id = 9, label = 'trigger_move' WHERE id = 6 RETURNING id, label, computed"),
        ]:
            cursor = reader.execute(command)
            changed = cursor.fetchall()
            rows = reader.execute("SELECT id, label, computed FROM move_write_diff ORDER BY id").fetchall()
            audit = reader.execute("SELECT event, leaf, id, label, computed FROM move_audit_diff ORDER BY event, id, label").fetchall()
            results.append(f"{label}=changed:{changed},rows:{rows},audit:{audit}")
        error = sqlstate(reader, "UPDATE move_high_diff SET id = 1 WHERE id = 11")
        rows = reader.execute("SELECT id, label, computed FROM move_write_diff ORDER BY id").fetchall()
        results.append(f"direct_leaf_move={error},rows:{rows}")
        reader.execute("BEGIN")
        reader.execute("UPDATE move_write_diff SET id = 1, label = 'rollback' WHERE id = 11")
        reader.execute("ROLLBACK")
        rows = reader.execute("SELECT id, label, computed FROM move_write_diff ORDER BY id").fetchall()
        audit = reader.execute("SELECT event, leaf, id, label, computed FROM move_audit_diff ORDER BY event, id, label").fetchall()
        results.append(f"move_rollback=rows:{rows},audit:{audit}")
        return [("reordered_" if reordered else "ordered_") + result for result in results]
    finally:
        reader.execute("ROLLBACK")
        writer.execute("DROP TABLE move_write_diff CASCADE")
        writer.execute("DROP TABLE move_audit_diff")
        writer.execute("DROP FUNCTION move_before_diff()")
        writer.execute("DROP FUNCTION move_after_diff()")


def merge_lock_transcript(reader, writer) -> list[str]:
    writer.execute("CREATE TABLE merge_lock_diff(id integer PRIMARY KEY, amount integer)")
    writer.execute("INSERT INTO merge_lock_diff VALUES (1, 10)")
    try:
        writer.execute("BEGIN")
        writer.execute("SELECT id FROM merge_lock_diff WHERE id = 1 FOR KEY SHARE")
        reader.execute("SET lock_timeout = '50ms'")
        changed = reader.execute("MERGE INTO merge_lock_diff t USING (VALUES (1)) AS s(id) ON t.id = s.id WHEN MATCHED THEN UPDATE SET amount = 11 RETURNING t.id, t.amount").fetchall()
        results = [f"merge_key_share_compatible={changed}"]
        for label, action in [("key", "UPDATE SET id = 2"), ("delete", "DELETE")]:
            error = sqlstate(reader, f"MERGE INTO merge_lock_diff t USING (VALUES (1)) AS s(id) ON t.id = s.id WHEN MATCHED THEN {action}")
            results.append(f"merge_key_share_{label}={error}")
        writer.execute("ROLLBACK")
        rows = reader.execute("SELECT id, amount FROM merge_lock_diff").fetchall()
        results.append(f"merge_locked_rows={rows}")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("ROLLBACK")
        reader.execute("RESET lock_timeout")
        writer.execute("DROP TABLE merge_lock_diff")


def merge_write_transcript(reader, writer) -> list[str]:
    writer.execute("CREATE TABLE merge_write_diff(id integer PRIMARY KEY, amount integer)")
    writer.execute("CREATE TABLE merge_reference_diff(id integer REFERENCES merge_write_diff(id) ON UPDATE CASCADE ON DELETE CASCADE)")
    writer.execute("INSERT INTO merge_write_diff VALUES (1, 10)")
    writer.execute("INSERT INTO merge_reference_diff VALUES (1)")
    try:
        changed = reader.execute("MERGE INTO merge_write_diff t USING (VALUES (1, 20)) AS s(id, amount) ON t.id = s.id WHEN MATCHED THEN UPDATE SET id = 2, amount = s.amount RETURNING t.id, t.amount").fetchall()
        referenced = reader.execute("SELECT id FROM merge_reference_diff").fetchall()
        results = [f"merge_key_actions=rows:{changed},references:{referenced}"]
        writer.execute("CREATE FUNCTION merge_rewrite_diff() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN IF NEW.amount = 30 THEN UPDATE merge_write_diff SET amount = 99 WHERE id = OLD.id; END IF; RETURN NEW; END$$")
        writer.execute("CREATE TRIGGER merge_rewrite_diff BEFORE UPDATE ON merge_write_diff FOR EACH ROW EXECUTE FUNCTION merge_rewrite_diff()")
        error = sqlstate(reader, "MERGE INTO merge_write_diff t USING (VALUES (2, 30)) AS s(id, amount) ON t.id = s.id WHEN MATCHED THEN UPDATE SET amount = s.amount")
        rows = reader.execute("SELECT id, amount FROM merge_write_diff").fetchall()
        results.append(f"merge_triggered_rewrite={error},rows:{rows}")
        reader.execute("MERGE INTO merge_write_diff t USING (VALUES (2)) AS s(id) ON t.id = s.id WHEN MATCHED THEN DELETE")
        referenced = reader.execute("SELECT id FROM merge_reference_diff").fetchall()
        results.append(f"merge_delete_actions={referenced}")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("DROP TABLE merge_reference_diff")
        writer.execute("DROP TABLE merge_write_diff")
        writer.execute("DROP FUNCTION IF EXISTS merge_rewrite_diff()")


def upsert_write_transcript(reader, writer) -> list[str]:
    writer.execute("CREATE TABLE upsert_write_target(id integer PRIMARY KEY, amount integer)")
    writer.execute("CREATE TABLE upsert_write_reference(id integer REFERENCES upsert_write_target(id) ON UPDATE CASCADE)")
    writer.execute("INSERT INTO upsert_write_target VALUES (1, 10)")
    writer.execute("INSERT INTO upsert_write_reference VALUES (1)")
    try:
        changed = reader.execute("INSERT INTO upsert_write_target VALUES (1, 20) ON CONFLICT(id) DO UPDATE SET id = 2, amount = excluded.amount RETURNING id, amount").fetchall()
        referenced = reader.execute("SELECT id FROM upsert_write_reference").fetchall()
        result = [f"upsert_key_actions=rows:{changed},references:{referenced}"]
        writer.execute("""CREATE FUNCTION upsert_write_rewrite() RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN
                IF NEW.amount = 30 THEN
                    UPDATE upsert_write_target SET amount = 99 WHERE id = OLD.id;
                END IF;
                RETURN NEW;
            END$$""")
        writer.execute("CREATE TRIGGER upsert_write_rewrite BEFORE UPDATE ON upsert_write_target FOR EACH ROW EXECUTE FUNCTION upsert_write_rewrite()")
        for label, command in [
            ("triggered", "INSERT INTO upsert_write_target VALUES (2, 30) ON CONFLICT(id) DO UPDATE SET amount = excluded.amount"),
            ("repeated", "INSERT INTO upsert_write_target VALUES (2, 21), (2, 22) ON CONFLICT(id) DO UPDATE SET amount = excluded.amount"),
        ]:
            error = sqlstate(reader, command)
            rows = reader.execute("SELECT id, amount FROM upsert_write_target").fetchall()
            result.append(f"upsert_{label}={error},rows:{rows}")
        return result
    finally:
        reader.execute("ROLLBACK")
        writer.execute("DROP TABLE upsert_write_reference")
        writer.execute("DROP TABLE upsert_write_target")
        writer.execute("DROP FUNCTION IF EXISTS upsert_write_rewrite()")


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


def prepared_statement_cleanup_transcript(reader) -> list[str]:
    # Rollback clears psycopg's cache through DEALLOCATE ALL. Repeatedly replace
    # every cached name without exceeding the server's startup reservation.
    for cycle in range(24):
        reader.execute("BEGIN")
        last = reader.execute(f"SELECT {cycle} AS cleanup_cycle", prepare=True).fetchone()[0]
        reader.execute("ROLLBACK")
    results = [f"prepared_cleanup_rollback=cycles:24,last:{last}"]
    for cycle in range(24):
        query = f"SELECT {cycle} AS explicit_cleanup_cycle"
        last = reader.execute(query, prepare=True).fetchone()[0]
        reader.execute("DEALLOCATE ALL", prepare=False)
        # The completion tag must also tell the driver to forget this name.
        assert reader.execute(query, prepare=True).fetchone()[0] == cycle
    results.append(f"prepared_cleanup_all=cycles:24,last:{last}")
    return results


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


def alter_rewrite_source_transcript(reader, writer) -> list[str]:
    writer.execute("CREATE TABLE alter_source_diff(id integer, amount bigint)")
    writer.execute("INSERT INTO alter_source_diff VALUES (1, 10), (2, 20), (3, 30)")
    results = []
    try:
        reader.execute("BEGIN")
        reader.execute("UPDATE alter_source_diff SET amount = amount + 100 WHERE id = 2")
        reader.execute("DELETE FROM alter_source_diff WHERE id = 3")
        reader.execute("ALTER TABLE alter_source_diff ALTER COLUMN amount TYPE text USING amount::text")
        changed = reader.execute("SELECT id, amount FROM alter_source_diff ORDER BY id").fetchall()
        reader.execute("ROLLBACK")
        restored = writer.execute("SELECT id, amount FROM alter_source_diff ORDER BY id").fetchall()
        results.append(f"alter_source_own=changed:{changed},restored:{restored}")
        writer.execute("ALTER TABLE alter_source_diff RENAME COLUMN amount TO total")
        writer.execute("ALTER TABLE alter_source_diff ALTER COLUMN total SET DEFAULT 5")
        writer.execute("ALTER TABLE alter_source_diff ALTER COLUMN total SET NOT NULL")
        writer.execute("INSERT INTO alter_source_diff(id) VALUES (4)")
        rows = writer.execute("SELECT id, total FROM alter_source_diff ORDER BY id").fetchall()
        results.append(f"alter_source_metadata={rows}")
        error = sqlstate(writer, "ALTER TABLE alter_source_diff ADD COLUMN required integer NOT NULL")
        count = writer.execute("SELECT count(*) FROM alter_source_diff").fetchone()[0]
        results.append(f"alter_source_required={error},rows:{count}")
        reader.execute("BEGIN")
        reader.execute("DELETE FROM alter_source_diff")
        reader.execute("ALTER TABLE alter_source_diff ADD COLUMN required integer NOT NULL")
        empty = reader.execute("SELECT count(*) FROM alter_source_diff").fetchone()[0]
        reader.execute("ROLLBACK")
        restored = writer.execute("SELECT id, total FROM alter_source_diff ORDER BY id").fetchall()
        results.append(f"alter_source_own_delete=empty:{empty},restored:{restored}")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("DROP TABLE alter_source_diff")


def row_layout_publication_transcript(reader, writer) -> list[str]:
    results = []
    for prepared_commit in (False, True):
        writer.execute("CREATE TABLE layout_publication_orders(id integer PRIMARY KEY)")
        writer.execute("CREATE TABLE layout_publication_invoices(id integer)")
        writer.execute("INSERT INTO layout_publication_orders VALUES (1), (2), (3), (4)")
        writer.execute("INSERT INTO layout_publication_invoices VALUES (22)")
        prepared = False
        try:
            reader.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
            assert reader.execute("SELECT id FROM layout_publication_invoices").fetchall() == [(22,)]
            writer.execute("DELETE FROM layout_publication_orders WHERE id > 1")
            writer.execute("BEGIN")
            writer.execute("ALTER TABLE layout_publication_orders ALTER COLUMN id TYPE bigint")
            batch = "not_applicable"
            if prepared_commit:
                writer.execute("PREPARE TRANSACTION 'layout-publication-diff'")
                prepared = True
                batch = sqlstate(writer, "SELECT 1; COMMIT PREPARED 'layout-publication-diff'")
                assert batch == "25001", batch
                writer.execute("COMMIT PREPARED 'layout-publication-diff'")
                prepared = False
            else:
                writer.execute("COMMIT")
            older = reader.execute("SELECT count(*) FROM layout_publication_orders").fetchone()[0]
            invoices = reader.execute("SELECT id FROM layout_publication_invoices").fetchall()
            current = writer.execute("SELECT id, pg_typeof(id)::text FROM layout_publication_orders").fetchall()
            reader.execute("ROLLBACK")
            writer.execute("UPDATE layout_publication_orders SET id = 2")
            updated = writer.execute("SELECT id FROM layout_publication_orders").fetchall()
            results.append(f"row_layout_prepared={prepared_commit},older:{older},invoices:{invoices},current:{current},updated:{updated},batch:{batch}")
        finally:
            reader.execute("ROLLBACK")
            writer.execute("ROLLBACK")
            if prepared:
                writer.execute("ROLLBACK PREPARED 'layout-publication-diff'")
            writer.execute("DROP TABLE layout_publication_orders")
            writer.execute("DROP TABLE layout_publication_invoices")
    return results


def copy_transition_value_transcript(reader, writer) -> list[str]:
    writer.execute("CREATE TABLE copy_value_diff(id integer, label text, chars integer GENERATED ALWAYS AS (length(label)) STORED)")
    writer.execute("CREATE TABLE copy_value_audit_diff(labels text, total bigint)")
    writer.execute("CREATE FUNCTION copy_value_before_diff() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN NEW.label := upper(NEW.label); RETURN NEW; END$$")
    writer.execute("CREATE FUNCTION copy_value_after_diff() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN UPDATE copy_value_diff SET label = label || '+' WHERE id < NEW.id; RETURN NULL; END$$")
    writer.execute("CREATE FUNCTION copy_value_statement_diff() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN INSERT INTO copy_value_audit_diff SELECT string_agg(label, ',' ORDER BY id), sum(chars) FROM inserted_rows; RETURN NULL; END$$")
    writer.execute("CREATE TRIGGER copy_value_before_diff BEFORE INSERT ON copy_value_diff FOR EACH ROW EXECUTE FUNCTION copy_value_before_diff()")
    writer.execute("CREATE TRIGGER copy_value_after_diff AFTER INSERT ON copy_value_diff FOR EACH ROW EXECUTE FUNCTION copy_value_after_diff()")
    writer.execute("CREATE TRIGGER copy_value_statement_diff AFTER INSERT ON copy_value_diff REFERENCING NEW TABLE AS inserted_rows FOR EACH STATEMENT EXECUTE FUNCTION copy_value_statement_diff()")
    results = []
    try:
        reader.execute("BEGIN")
        with reader.cursor().copy("COPY copy_value_diff(id, label) FROM STDIN") as copied:
            copied.write(b"1\tone\n2\tt")
            assert writer.execute("SELECT 42").fetchone() == (42,)
            copied.write(b"wo\n3\tthree\n")
        current = reader.execute("SELECT id, label, chars FROM copy_value_diff ORDER BY id").fetchall()
        captured = reader.execute("SELECT labels, total FROM copy_value_audit_diff").fetchall()
        results.append(f"copy_values_text=rows:{current},transition:{captured}")
        reader.execute("ROLLBACK")
        empty = writer.execute("SELECT count(*) FROM copy_value_diff").fetchone()[0]
        no_audit = writer.execute("SELECT count(*) FROM copy_value_audit_diff").fetchone()[0]
        results.append(f"copy_values_rollback=rows:{empty},audit:{no_audit}")
        with reader.cursor().copy("COPY copy_value_diff(id, label) FROM STDIN (FORMAT binary)") as copied:
            copied.set_types(["int4", "text"])
            copied.write_row((4, "four"))
            copied.write_row((5, "five"))
        current = reader.execute("SELECT id, label, chars FROM copy_value_diff ORDER BY id").fetchall()
        captured = reader.execute("SELECT labels, total FROM copy_value_audit_diff").fetchall()
        results.append(f"copy_values_binary=rows:{current},transition:{captured}")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("DROP TABLE copy_value_diff CASCADE")
        writer.execute("DROP TABLE copy_value_audit_diff")
        for name in ("before", "after", "statement"):
            writer.execute(f"DROP FUNCTION copy_value_{name}_diff()")


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


def rule_publication_transcript(reader, writer) -> list[str]:
    results = []
    writer.execute("CREATE TABLE rule_publication_diff(id integer)")
    writer.execute("CREATE TABLE rule_publication_other_diff(id integer)")
    writer.execute("CREATE VIEW rule_publication_view_diff AS SELECT id FROM rule_publication_diff")
    reader.execute("SET lock_timeout = '50ms'")
    writer.execute("SET lock_timeout = '50ms'")
    try:
        for relation in ["rule_publication_diff", "rule_publication_view_diff"]:
            writer.execute("BEGIN")
            writer.execute(f"CREATE RULE first_rule AS ON INSERT TO {relation} DO INSTEAD NOTHING")
            results.append(f"rule_create_competes_{relation}=" + str(sqlstate(reader, f"CREATE RULE second_rule AS ON INSERT TO {relation} DO INSTEAD NOTHING")))
            results.append(f"rule_read_competes_{relation}=" + str(sqlstate(reader, f"SELECT * FROM {relation}")))
            results.append(f"rule_write_competes_{relation}=" + str(sqlstate(reader, f"INSERT INTO {relation} VALUES (1)")))
            reader.execute("CREATE RULE unrelated_rule AS ON INSERT TO rule_publication_other_diff DO INSTEAD NOTHING")
            reader.execute("DROP RULE unrelated_rule ON rule_publication_other_diff")
            results.append(f"rule_unrelated_{relation}=ok")
            writer.execute("ROLLBACK")
            results.append(f"rule_after_rollback_{relation}=" + str(reader.execute("SELECT relhasrules FROM pg_class WHERE relname = %s", (relation,)).fetchone()[0]))
            reader.execute(f"DROP RULE IF EXISTS second_rule ON {relation}")

        writer.execute("BEGIN")
        writer.execute("SAVEPOINT before_rule")
        writer.execute("CREATE RULE savepoint_rule AS ON INSERT TO rule_publication_diff DO INSTEAD NOTHING")
        results.append("rule_before_savepoint_rollback=" + str(sqlstate(reader, "CREATE RULE retry_rule AS ON INSERT TO rule_publication_diff DO INSTEAD NOTHING")))
        writer.execute("ROLLBACK TO SAVEPOINT before_rule")
        reader.execute("DROP RULE IF EXISTS retry_rule ON rule_publication_diff")
        results.append("rule_savepoint_history=" + str(writer.execute("SELECT relhasrules FROM pg_class WHERE relname = 'rule_publication_diff'").fetchone()[0]))
        reader.execute("CREATE RULE retry_rule AS ON INSERT TO rule_publication_diff DO INSTEAD NOTHING")
        reader.execute("DROP RULE retry_rule ON rule_publication_diff")
        writer.execute("ROLLBACK")
        results.append("rule_committed_history=" + str(reader.execute("SELECT relhasrules FROM pg_class WHERE relname = 'rule_publication_diff'").fetchone()[0]))

        reader.execute("CREATE RULE stable_rule AS ON INSERT TO rule_publication_diff DO INSTEAD NOTHING")
        for label, statement in [
            ("replace", "CREATE OR REPLACE RULE stable_rule AS ON INSERT TO rule_publication_diff DO INSTEAD NOTHING"),
            ("rename", "ALTER RULE stable_rule ON rule_publication_diff RENAME TO renamed_rule"),
            ("drop", "DROP RULE stable_rule ON rule_publication_diff"),
            ("disable", "ALTER TABLE rule_publication_diff DISABLE RULE stable_rule"),
        ]:
            reader.execute("BEGIN")
            reader.execute("SELECT * FROM rule_publication_diff")
            results.append(f"rule_existing_reader_{label}=" + str(sqlstate(writer, statement)))
            reader.execute("ROLLBACK")
        reader.execute("CREATE RULE stable_view_rule AS ON INSERT TO rule_publication_view_diff DO INSTEAD NOTHING")
        user_rules = reader.execute("SELECT rulename FROM pg_rules WHERE tablename='rule_publication_view_diff' ORDER BY rulename").fetchall()
        all_rules = reader.execute("SELECT rulename FROM pg_rewrite WHERE ev_class='rule_publication_view_diff'::regclass ORDER BY rulename").fetchall()
        results.append(f"rule_view_user_catalog={user_rules}")
        results.append(f"rule_view_rewrite_catalog={all_rules}")
        reader.execute("BEGIN")
        reader.execute("SELECT * FROM rule_publication_view_diff")
        reader.execute("SELECT id FROM rule_publication_diff FOR UPDATE")
        view_locks = reader.execute("SELECT mode, database=(SELECT oid FROM pg_database WHERE datname=current_database()) FROM pg_locks WHERE pid=pg_backend_pid() AND relation='rule_publication_view_diff'::regclass ORDER BY mode").fetchall()
        results.append(f"rule_view_lock_identity={view_locks}")
        for label, statement in [
            ("replace", "CREATE OR REPLACE VIEW rule_publication_view_diff AS SELECT id FROM rule_publication_diff"),
            ("rename", "ALTER VIEW rule_publication_view_diff RENAME TO renamed_view"),
            ("drop", "DROP VIEW rule_publication_view_diff"),
            ("disable-rule", "ALTER TABLE rule_publication_view_diff DISABLE RULE stable_view_rule"),
            ("disable-trigger", "ALTER TABLE rule_publication_view_diff DISABLE TRIGGER missing_trigger"),
            ("table-trigger-mode", "ALTER TABLE rule_publication_diff DISABLE TRIGGER missing_trigger"),
        ]:
            results.append(f"view_existing_reader_{label}=" + str(sqlstate(writer, statement)))
        reader.execute("ROLLBACK")
        results.append("view_after_reader_disable-rule=" + str(sqlstate(
            writer, "ALTER TABLE rule_publication_view_diff DISABLE RULE stable_view_rule"
        )))
        writer.execute("BEGIN")
        writer.execute("ALTER VIEW rule_publication_view_diff RENAME TO rule_publication_renamed_diff")
        renamed_rules = writer.execute("SELECT tablename,rulename FROM pg_rules WHERE tablename IN ('rule_publication_view_diff','rule_publication_renamed_diff') ORDER BY tablename,rulename").fetchall()
        definition_renamed = writer.execute("SELECT pg_get_ruledef(oid) LIKE '%rule_publication_renamed_diff%' FROM pg_rewrite WHERE ev_class='rule_publication_renamed_diff'::regclass AND rulename='stable_view_rule'").fetchone()[0]
        results.append(f"rule_pending_view_name={renamed_rules}")
        results.append(f"rule_pending_view_definition={definition_renamed}")
        writer.execute("ALTER VIEW rule_publication_renamed_diff SET (security_barrier=true)")
        visible_options = writer.execute("SELECT 'security_barrier=true'=ANY(reloptions) FROM pg_class WHERE relname='rule_publication_renamed_diff'").fetchone()[0]
        results.append(f"rule_pending_view_options={visible_options}")
        writer.execute("DROP RULE stable_view_rule ON rule_publication_renamed_diff")
        writer.execute("ROLLBACK")
        restored_rules = reader.execute("SELECT tablename,rulename FROM pg_rules WHERE tablename='rule_publication_view_diff' ORDER BY rulename").fetchall()
        results.append(f"rule_renamed_view_rollback={restored_rules}")
        writer.execute("BEGIN")
        writer.execute("ALTER VIEW rule_publication_view_diff RENAME TO rule_publication_renamed_diff")
        writer.execute("ALTER VIEW rule_publication_renamed_diff RENAME TO rule_publication_final_diff")
        writer.execute("DROP VIEW rule_publication_final_diff")
        missing = writer.execute("SELECT to_regclass('rule_publication_final_diff') IS NULL").fetchone()[0]
        results.append(f"rule_twice_renamed_view_drop={missing}")
        writer.execute("ROLLBACK")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("ROLLBACK")
        reader.execute("RESET lock_timeout")
        writer.execute("RESET lock_timeout")
        writer.execute("DROP VIEW rule_publication_view_diff")
        writer.execute("DROP TABLE rule_publication_diff")
        writer.execute("DROP TABLE rule_publication_other_diff")


def index_namespace_transcript(reader, writer) -> list[str]:
    results = []
    writer.execute("CREATE SCHEMA index_namespace_destination_diff")
    writer.execute("CREATE TABLE index_namespace_source_diff(id integer)")
    writer.execute("CREATE TABLE index_namespace_other_diff(id integer)")
    writer.execute("CREATE TABLE index_namespace_destination_diff.other_table(id integer)")
    writer.execute("CREATE INDEX index_namespace_old_diff ON index_namespace_source_diff(id)")
    writer.execute("CREATE INDEX index_namespace_competitor_diff ON index_namespace_other_diff(id)")
    writer.execute("CREATE INDEX index_namespace_competitor_diff ON index_namespace_destination_diff.other_table(id)")
    reader.execute("SET lock_timeout = '50ms'")
    try:
        for operation, statement, requests in [
            ("rename", "ALTER INDEX index_namespace_old_diff RENAME TO index_namespace_new_diff", [
                ("rename_old", "ALTER INDEX public.index_namespace_competitor_diff RENAME TO index_namespace_old_diff"),
                ("rename_new", "ALTER INDEX public.index_namespace_competitor_diff RENAME TO index_namespace_new_diff"),
                ("create_old", "CREATE INDEX index_namespace_old_diff ON index_namespace_other_diff(id)"),
                ("create_new", "CREATE INDEX index_namespace_new_diff ON index_namespace_other_diff(id)"),
            ]),
            ("drop", "DROP INDEX index_namespace_old_diff", [
                ("rename_old", "ALTER INDEX public.index_namespace_competitor_diff RENAME TO index_namespace_old_diff"),
                ("create_old", "CREATE INDEX index_namespace_old_diff ON index_namespace_other_diff(id)"),
            ]),
            ("schema", "ALTER TABLE index_namespace_source_diff SET SCHEMA index_namespace_destination_diff", [
                ("rename_old", "ALTER INDEX public.index_namespace_competitor_diff RENAME TO index_namespace_old_diff"),
                ("rename_new", "ALTER INDEX index_namespace_destination_diff.index_namespace_competitor_diff RENAME TO index_namespace_old_diff"),
                ("create_old", "CREATE INDEX index_namespace_old_diff ON index_namespace_other_diff(id)"),
                ("create_new", "CREATE INDEX index_namespace_old_diff ON index_namespace_destination_diff.other_table(id)"),
            ]),
        ]:
            writer.execute("BEGIN")
            writer.execute(statement)
            try:
                for label, query in requests:
                    results.append(f"index_namespace_{operation}_{label}=" + sqlstate(reader, query))
            finally:
                writer.execute("ROLLBACK")
        return results
    finally:
        reader.execute("ROLLBACK")
        writer.execute("ROLLBACK")
        writer.execute("DROP TABLE index_namespace_source_diff CASCADE")
        writer.execute("DROP TABLE index_namespace_other_diff CASCADE")
        writer.execute("DROP SCHEMA index_namespace_destination_diff CASCADE")


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
        ] + [
            line
            for transcript in [
                transaction_admission_transcript(reader, writer),
                dml_target_transcript(reader, writer),
                dml_description_transcript(reader, writer),
                prepared_statement_cleanup_transcript(reader),
                on_conflict_snapshot_transcript(reader, writer),
                upsert_write_transcript(reader, writer),
                generated_write_transcript(reader, writer),
                merge_write_transcript(reader, writer),
                merge_lock_transcript(reader, writer),
                partition_move_transcript(reader, writer),
                partition_move_transcript(reader, writer, reordered=True),
                partition_attachment_transcript(reader, writer),
                wide_partition_transcript(reader, writer),
                index_validation_transcript(reader, writer),
                index_namespace_transcript(reader, writer),
                partition_index_validation_transcript(reader, writer),
                constraint_validation_transcript(reader, writer),
                constraint_partition_transcript(reader, writer),
                constraint_rewrite_transcript(reader, writer),
                alter_rewrite_source_transcript(reader, writer),
                row_layout_publication_transcript(reader, writer),
                copy_transition_value_transcript(reader, writer),
                analyze_snapshot_transcript(reader, writer),
                rule_publication_transcript(reader, writer),
            ]
            for line in transcript
        ]
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
