CREATE TABLE row_trigger_target (
  id integer PRIMARY KEY,
  value integer NOT NULL,
  doubled integer GENERATED ALWAYS AS (value * 2) STORED,
  CHECK (value > 0)
);
CREATE TABLE row_trigger_audit (id integer, observed integer);
CREATE FUNCTION row_trigger_normalize() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN NEW.value := NEW.value + 1; RETURN NEW; END';
CREATE FUNCTION row_trigger_audit_write() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO row_trigger_audit VALUES (NEW.id, NEW.value); RETURN NEW; END';
CREATE FUNCTION row_trigger_audit_delete() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO row_trigger_audit VALUES (OLD.id, -OLD.value); RETURN OLD; END';
CREATE TRIGGER a_row_trigger_normalize BEFORE INSERT OR UPDATE ON row_trigger_target
  FOR EACH ROW EXECUTE FUNCTION row_trigger_normalize();
CREATE TRIGGER b_row_trigger_audit_write BEFORE INSERT OR UPDATE ON row_trigger_target
  FOR EACH ROW EXECUTE FUNCTION row_trigger_audit_write();
CREATE TRIGGER row_trigger_audit_delete BEFORE DELETE ON row_trigger_target
  FOR EACH ROW EXECUTE FUNCTION row_trigger_audit_delete();
INSERT INTO row_trigger_target VALUES (1, 4);
UPDATE row_trigger_target SET value = 8 WHERE id = 1;
SELECT id, value, doubled FROM row_trigger_target;
DELETE FROM row_trigger_target WHERE id = 1;
SELECT id, observed FROM row_trigger_audit ORDER BY observed;
