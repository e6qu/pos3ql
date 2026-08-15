CREATE TABLE row_trigger_target (
  id integer PRIMARY KEY,
  value integer NOT NULL,
  doubled integer GENERATED ALWAYS AS (value * 2) STORED,
  CHECK (value > 0)
);
CREATE TABLE row_trigger_audit (id integer, observed integer);
CREATE TABLE row_trigger_after (id integer, observed integer);
CREATE FUNCTION row_trigger_normalize() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN NEW.value := NEW.value + 1; RETURN NEW; END';
CREATE FUNCTION row_trigger_audit_write() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO row_trigger_audit VALUES (NEW.id, NEW.value); RETURN NEW; END';
CREATE FUNCTION row_trigger_audit_delete() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO row_trigger_audit VALUES (OLD.id, -OLD.value); RETURN OLD; END';
CREATE FUNCTION row_trigger_after_write() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO row_trigger_after VALUES (NEW.id, NEW.value); RETURN NULL; END';
CREATE FUNCTION row_trigger_after_delete() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO row_trigger_after VALUES (OLD.id, -OLD.value); RETURN NULL; END';
CREATE TRIGGER a_row_trigger_normalize BEFORE INSERT OR UPDATE ON row_trigger_target
  FOR EACH ROW EXECUTE FUNCTION row_trigger_normalize();
CREATE TRIGGER b_row_trigger_audit_write BEFORE INSERT OR UPDATE ON row_trigger_target
  FOR EACH ROW EXECUTE FUNCTION row_trigger_audit_write();
CREATE TRIGGER row_trigger_audit_delete BEFORE DELETE ON row_trigger_target
  FOR EACH ROW EXECUTE FUNCTION row_trigger_audit_delete();
CREATE TRIGGER row_trigger_after_write AFTER INSERT OR UPDATE ON row_trigger_target
  FOR EACH ROW EXECUTE FUNCTION row_trigger_after_write();
CREATE TRIGGER row_trigger_after_delete AFTER DELETE ON row_trigger_target
  FOR EACH ROW EXECUTE FUNCTION row_trigger_after_delete();
INSERT INTO row_trigger_target VALUES (1, 4);
UPDATE row_trigger_target SET value = 8 WHERE id = 1;
SELECT id, value, doubled FROM row_trigger_target;
DELETE FROM row_trigger_target WHERE id = 1;
SELECT count(*) FROM row_trigger_target;
SELECT id, observed FROM row_trigger_audit ORDER BY observed;
SELECT id, observed FROM row_trigger_after ORDER BY observed;

CREATE TABLE row_trigger_program_target (id integer PRIMARY KEY, value integer);
CREATE TABLE row_trigger_program_side (id integer PRIMARY KEY, value integer);
CREATE TABLE row_trigger_program_audit (value integer);
INSERT INTO row_trigger_program_side VALUES (1, 10), (2, 20);
CREATE FUNCTION row_trigger_program() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN IF NEW.value > 5 THEN
           UPDATE row_trigger_program_side SET value = NEW.value WHERE id = NEW.id;
         ELSIF NEW.value = 5 THEN
           DELETE FROM row_trigger_program_side WHERE id = NEW.value - 3;
         ELSE
           BEGIN INSERT INTO row_trigger_program_audit VALUES (NEW.value); END;
         END IF;
         RETURN NEW; END';
CREATE TRIGGER row_trigger_program_before BEFORE INSERT ON row_trigger_program_target
  FOR EACH ROW EXECUTE FUNCTION row_trigger_program();
INSERT INTO row_trigger_program_target VALUES (1, 7), (2, 5), (3, 2);
SELECT id, value FROM row_trigger_program_side ORDER BY id;
SELECT value FROM row_trigger_program_audit;

CREATE TABLE row_trigger_join_target (id integer PRIMARY KEY, value integer);
CREATE TABLE row_trigger_join_source (id integer, delta integer, enabled boolean);
CREATE TABLE row_trigger_join_driver (id integer PRIMARY KEY, value integer);
INSERT INTO row_trigger_join_target VALUES (1, 10), (2, 20);
INSERT INTO row_trigger_join_source VALUES (1, 2, true), (2, 3, true);
INSERT INTO row_trigger_join_driver VALUES (1, 5);
CREATE FUNCTION row_trigger_join_program() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN
     UPDATE row_trigger_join_target
        SET value = row_trigger_join_target.value + row_trigger_join_source.delta + NEW.value - OLD.value
       FROM row_trigger_join_source
      WHERE row_trigger_join_target.id = row_trigger_join_source.id
        AND row_trigger_join_source.enabled
        AND OLD.id = NEW.id;
     DELETE FROM row_trigger_join_target
           USING row_trigger_join_source
      WHERE row_trigger_join_target.id = row_trigger_join_source.id
        AND row_trigger_join_source.delta = 3
        AND OLD.value = 5
        AND NEW.value = 7;
     RETURN NEW;
   END';
CREATE TRIGGER row_trigger_join_after AFTER UPDATE ON row_trigger_join_driver
  FOR EACH ROW EXECUTE FUNCTION row_trigger_join_program();
UPDATE row_trigger_join_driver SET value = 7 WHERE id = 1;
SELECT id, value FROM row_trigger_join_target ORDER BY id;

CREATE TABLE row_trigger_qualification_target (id integer PRIMARY KEY, value integer, note integer);
CREATE TABLE row_trigger_qualification_audit (id integer, value integer);
CREATE FUNCTION row_trigger_qualification() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO row_trigger_qualification_audit VALUES (NEW.id, NEW.value); RETURN NEW; END';
CREATE TRIGGER row_trigger_qualification_before
  BEFORE UPDATE OF value ON row_trigger_qualification_target
  FOR EACH ROW WHEN (NEW.value > OLD.value)
  EXECUTE FUNCTION row_trigger_qualification();
INSERT INTO row_trigger_qualification_target VALUES (1, 3, 0);
UPDATE row_trigger_qualification_target SET note = 1 WHERE id = 1;
UPDATE row_trigger_qualification_target SET value = 2 WHERE id = 1;
UPDATE row_trigger_qualification_target SET value = 7 WHERE id = 1;
SELECT id, value FROM row_trigger_qualification_audit;

CREATE TABLE statement_trigger_target (id integer PRIMARY KEY, value integer);
CREATE TABLE statement_trigger_audit (event text, value integer);
CREATE FUNCTION statement_trigger_note() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO statement_trigger_audit VALUES (''statement'', 0); RETURN NULL; END';
CREATE FUNCTION statement_conflict_note() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO statement_trigger_audit VALUES (''update'', NEW.value); RETURN NEW; END';
CREATE TRIGGER statement_insert BEFORE INSERT ON statement_trigger_target
  EXECUTE FUNCTION statement_trigger_note();
CREATE TRIGGER statement_update AFTER UPDATE OF value ON statement_trigger_target
  FOR EACH STATEMENT EXECUTE FUNCTION statement_trigger_note();
CREATE TRIGGER statement_truncate AFTER TRUNCATE ON statement_trigger_target
  FOR EACH STATEMENT EXECUTE FUNCTION statement_trigger_note();
CREATE TRIGGER statement_conflict_update BEFORE UPDATE OF value ON statement_trigger_target
  FOR EACH ROW WHEN (NEW.value > OLD.value) EXECUTE FUNCTION statement_conflict_note();
INSERT INTO statement_trigger_target VALUES (1, 1), (2, 2);
UPDATE statement_trigger_target SET value = value + 1;
INSERT INTO statement_trigger_target VALUES (1, 9)
  ON CONFLICT (id) DO UPDATE SET value = excluded.value;
TRUNCATE statement_trigger_target;
SELECT event, value FROM statement_trigger_audit ORDER BY value, event;

CREATE TABLE transition_trigger_target (id integer PRIMARY KEY, value integer);
CREATE TABLE transition_trigger_audit (kind text, id integer, value integer);
CREATE FUNCTION transition_trigger_update() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN
     INSERT INTO transition_trigger_audit SELECT ''old'', id, value FROM old_rows;
     INSERT INTO transition_trigger_audit SELECT ''new'', id, value FROM new_rows;
     RETURN NULL;
   END';
CREATE FUNCTION transition_trigger_delete() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO transition_trigger_audit SELECT ''delete'', id, value FROM deleted_rows; RETURN NULL; END';
CREATE TRIGGER transition_trigger_update_after AFTER UPDATE ON transition_trigger_target
  REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows FOR EACH STATEMENT
  EXECUTE FUNCTION transition_trigger_update();
CREATE TRIGGER transition_trigger_delete_after AFTER DELETE ON transition_trigger_target
  REFERENCING OLD TABLE AS deleted_rows FOR EACH STATEMENT
  EXECUTE FUNCTION transition_trigger_delete();
INSERT INTO transition_trigger_target VALUES (1, 10), (2, 20);
UPDATE transition_trigger_target SET value = value + 1;
DELETE FROM transition_trigger_target WHERE id = 2;
SELECT kind, id, value FROM transition_trigger_audit ORDER BY kind, id;

CREATE TABLE trigger_local_query_target (id integer PRIMARY KEY, value integer);
CREATE TABLE trigger_local_query_source (id integer PRIMARY KEY, delta integer);
CREATE TABLE trigger_local_query_audit (value integer);
INSERT INTO trigger_local_query_target VALUES (1, 5);
INSERT INTO trigger_local_query_source VALUES (1, 3);
CREATE FUNCTION trigger_local_query_program() RETURNS trigger LANGUAGE plpgsql AS
  'DECLARE change integer := NEW.value - OLD.value;
           selected_delta integer;
   BEGIN
     SELECT source.delta INTO selected_delta
       FROM trigger_local_query_source source WHERE source.id = NEW.id;
     selected_delta := selected_delta + 1;
     PERFORM 1 FROM trigger_local_query_source source
       WHERE source.id = NEW.id AND selected_delta = source.delta + 1;
     INSERT INTO trigger_local_query_audit VALUES (selected_delta);
     NEW.value := NEW.value + change + selected_delta;
     RETURN NEW;
   END';
CREATE TRIGGER trigger_local_query_before BEFORE UPDATE ON trigger_local_query_target
  FOR EACH ROW EXECUTE FUNCTION trigger_local_query_program();
UPDATE trigger_local_query_target SET value = 7 WHERE id = 1;
SELECT value FROM trigger_local_query_target;
SELECT value FROM trigger_local_query_audit;

CREATE TABLE trigger_local_transition_target (id integer PRIMARY KEY, value integer);
CREATE TABLE trigger_local_transition_audit (changed integer);
CREATE FUNCTION trigger_local_transition_program() RETURNS trigger LANGUAGE plpgsql AS
  'DECLARE changed_count integer;
   BEGIN
     SELECT count(*) INTO changed_count FROM changed_rows;
     PERFORM 1 FROM changed_rows WHERE value > 0;
     INSERT INTO trigger_local_transition_audit VALUES (changed_count);
     RETURN NULL;
   END';
CREATE TRIGGER trigger_local_transition_after AFTER UPDATE ON trigger_local_transition_target
  REFERENCING NEW TABLE AS changed_rows FOR EACH STATEMENT
  EXECUTE FUNCTION trigger_local_transition_program();
INSERT INTO trigger_local_transition_target VALUES (1, 2), (2, 3);
UPDATE trigger_local_transition_target SET value = value + 1;
SELECT changed FROM trigger_local_transition_audit;

CREATE TABLE trigger_dml_scope_driver (id integer PRIMARY KEY, value integer);
CREATE TABLE trigger_dml_scope_target (id integer PRIMARY KEY, value integer);
CREATE TABLE trigger_dml_scope_source (id integer PRIMARY KEY, delta integer);
INSERT INTO trigger_dml_scope_driver VALUES (1, 5);
INSERT INTO trigger_dml_scope_target VALUES (1, 10), (2, 20);
INSERT INTO trigger_dml_scope_source VALUES (1, 3), (2, 4);
CREATE FUNCTION trigger_dml_scope_program() RETURNS trigger LANGUAGE plpgsql AS
  'DECLARE step integer := NEW.value - OLD.value;
   BEGIN
     UPDATE trigger_dml_scope_target
        SET value = trigger_dml_scope_target.value + trigger_dml_scope_source.delta + step
       FROM trigger_dml_scope_source
      WHERE trigger_dml_scope_target.id = trigger_dml_scope_source.id
        AND trigger_dml_scope_source.id = NEW.id
        AND step = 2;
     DELETE FROM trigger_dml_scope_target
       USING trigger_dml_scope_source
      WHERE trigger_dml_scope_target.id = trigger_dml_scope_source.id
        AND trigger_dml_scope_source.id = 2
        AND step = 2;
     RETURN NEW;
   END';
CREATE TRIGGER trigger_dml_scope_before BEFORE UPDATE ON trigger_dml_scope_driver
  FOR EACH ROW EXECUTE FUNCTION trigger_dml_scope_program();
UPDATE trigger_dml_scope_driver SET value = 7 WHERE id = 1;
SELECT id, value FROM trigger_dml_scope_target ORDER BY id;

CREATE TABLE trigger_transition_dml_driver (id integer PRIMARY KEY, delta integer);
CREATE TABLE trigger_transition_dml_target (id integer PRIMARY KEY, value integer);
INSERT INTO trigger_transition_dml_driver VALUES (1, 3), (2, 9);
INSERT INTO trigger_transition_dml_target VALUES (1, 10), (2, 20);
CREATE FUNCTION trigger_transition_dml_program() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN
     UPDATE trigger_transition_dml_target
        SET value = trigger_transition_dml_target.value + changed_rows.delta
       FROM changed_rows
      WHERE trigger_transition_dml_target.id = changed_rows.id;
     DELETE FROM trigger_transition_dml_target
       USING changed_rows
      WHERE trigger_transition_dml_target.id = changed_rows.id
        AND changed_rows.delta = 9;
     RETURN NULL;
   END';
CREATE TRIGGER trigger_transition_dml_after AFTER UPDATE ON trigger_transition_dml_driver
  REFERENCING NEW TABLE AS changed_rows FOR EACH STATEMENT
  EXECUTE FUNCTION trigger_transition_dml_program();
UPDATE trigger_transition_dml_driver SET delta = delta;
SELECT id, value FROM trigger_transition_dml_target ORDER BY id;

CREATE TABLE trigger_insert_select_driver (id integer PRIMARY KEY, value integer);
CREATE TABLE trigger_insert_select_source (id integer PRIMARY KEY, value integer);
CREATE TABLE trigger_insert_select_target (id integer PRIMARY KEY, value integer);
INSERT INTO trigger_insert_select_driver VALUES (1, 5);
INSERT INTO trigger_insert_select_source VALUES (1, 10), (2, 20);
CREATE FUNCTION trigger_insert_select_program() RETURNS trigger LANGUAGE plpgsql AS
  'DECLARE step integer := NEW.value - OLD.value;
   BEGIN
     INSERT INTO trigger_insert_select_target
       SELECT source.id, source.value + step
         FROM trigger_insert_select_source source
        WHERE source.id = NEW.id AND step = 2;
     RETURN NEW;
   END';
CREATE TRIGGER trigger_insert_select_before BEFORE UPDATE ON trigger_insert_select_driver
  FOR EACH ROW EXECUTE FUNCTION trigger_insert_select_program();
UPDATE trigger_insert_select_driver SET value = 7 WHERE id = 1;
SELECT id, value FROM trigger_insert_select_target ORDER BY id;

CREATE TABLE trigger_conflict_driver (id integer PRIMARY KEY, value integer);
CREATE TABLE trigger_conflict_target (id integer PRIMARY KEY, value integer);
INSERT INTO trigger_conflict_driver VALUES (1, 5);
INSERT INTO trigger_conflict_target VALUES (1, 10);
CREATE FUNCTION trigger_conflict_program() RETURNS trigger LANGUAGE plpgsql AS
  'DECLARE step integer := NEW.value - OLD.value;
   BEGIN
     INSERT INTO trigger_conflict_target VALUES (NEW.id, NEW.value)
       ON CONFLICT (id) DO UPDATE
          SET value = excluded.value + step
        WHERE step = 2 AND OLD.value = 5 AND NEW.value = 7;
     RETURN NEW;
   END';
CREATE TRIGGER trigger_conflict_after AFTER UPDATE ON trigger_conflict_driver
  FOR EACH ROW EXECUTE FUNCTION trigger_conflict_program();
UPDATE trigger_conflict_driver SET value = 7 WHERE id = 1;
SELECT id, value FROM trigger_conflict_target ORDER BY id;
