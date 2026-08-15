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
