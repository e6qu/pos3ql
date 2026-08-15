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
