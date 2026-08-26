-- Trigger identity, transition relations, partition clones, and constraint timing.
DROP TABLE IF EXISTS trigger_timing_target, trigger_timing_audit,
  trigger_timing_order,
  trigger_timing_constraint_target, trigger_timing_constraint_audit,
  trigger_timing_root, trigger_timing_low, trigger_timing_high CASCADE;

CREATE TABLE trigger_timing_target (id integer PRIMARY KEY, value integer);
CREATE TABLE trigger_timing_audit
  (event text, id integer, old_rows integer, new_rows integer);
CREATE TABLE trigger_timing_order
  (position serial, name text, id integer, transition_count integer);
CREATE FUNCTION trigger_timing_transition() RETURNS trigger LANGUAGE plpgsql AS
  'DECLARE old_count integer; new_count integer;
   BEGIN
     SELECT count(*) INTO old_count FROM old_set;
     SELECT count(*) INTO new_count FROM new_set;
     INSERT INTO trigger_timing_audit VALUES (TG_OP, NEW.id, old_count, new_count);
     INSERT INTO trigger_timing_order (name, id, transition_count)
       VALUES (TG_NAME, NEW.id, new_count);
     RETURN NEW;
   END';
CREATE TRIGGER trigger_timing_after AFTER UPDATE ON trigger_timing_target
  REFERENCING OLD TABLE AS old_set NEW TABLE AS new_set FOR EACH ROW
  EXECUTE FUNCTION trigger_timing_transition();
CREATE FUNCTION trigger_timing_completed_query() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN
     INSERT INTO trigger_timing_audit
       VALUES (''ordinary'', NEW.id,
               (SELECT count(*) FROM trigger_timing_target WHERE value > 10), NULL);
     INSERT INTO trigger_timing_order (name, id) VALUES (TG_NAME, NEW.id);
     RETURN NEW;
   END';
CREATE TRIGGER trigger_timing_a_ordinary AFTER UPDATE ON trigger_timing_target
  FOR EACH ROW EXECUTE FUNCTION trigger_timing_completed_query();
CREATE CONSTRAINT TRIGGER trigger_timing_c_constraint AFTER UPDATE
  ON trigger_timing_target NOT DEFERRABLE FOR EACH ROW
  EXECUTE FUNCTION trigger_timing_completed_query();
CREATE FUNCTION trigger_timing_statement() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN
     INSERT INTO trigger_timing_order (name) VALUES (TG_NAME);
     RETURN NULL;
   END';
CREATE TRIGGER trigger_timing_z_statement AFTER UPDATE ON trigger_timing_target
  FOR EACH STATEMENT EXECUTE FUNCTION trigger_timing_statement();
COMMENT ON TRIGGER trigger_timing_after ON trigger_timing_target IS 'transition rows';
INSERT INTO trigger_timing_target VALUES (1, 10), (2, 20);
UPDATE trigger_timing_target SET value = value + 1;
SELECT event, id, old_rows, new_rows FROM trigger_timing_audit
ORDER BY event COLLATE "C", id;
SELECT name, id, transition_count FROM trigger_timing_order ORDER BY position;
SELECT obj_description(oid, 'pg_trigger'), pg_get_triggerdef(oid),
       tgoldtable, tgnewtable
FROM pg_trigger WHERE tgname = 'trigger_timing_after';

CREATE TABLE trigger_timing_constraint_target (id integer PRIMARY KEY);
CREATE TABLE trigger_timing_constraint_audit (id integer);
CREATE FUNCTION trigger_timing_constraint() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO trigger_timing_constraint_audit VALUES (NEW.id); RETURN NEW; END';
CREATE CONSTRAINT TRIGGER trigger_timing_deferred AFTER INSERT
  ON trigger_timing_constraint_target DEFERRABLE INITIALLY DEFERRED
  FOR EACH ROW EXECUTE FUNCTION trigger_timing_constraint();
BEGIN;
INSERT INTO trigger_timing_constraint_target VALUES (1);
SAVEPOINT queued_trigger;
SET CONSTRAINTS trigger_timing_deferred IMMEDIATE;
SELECT id FROM trigger_timing_constraint_audit;
ROLLBACK TO SAVEPOINT queued_trigger;
SELECT count(*) FROM trigger_timing_constraint_audit;
COMMIT;
SELECT id FROM trigger_timing_constraint_audit;

CREATE TABLE trigger_timing_root (id integer) PARTITION BY RANGE (id);
CREATE TABLE trigger_timing_low PARTITION OF trigger_timing_root
  FOR VALUES FROM (0) TO (100);
CREATE TABLE trigger_timing_high PARTITION OF trigger_timing_root
  FOR VALUES FROM (100) TO (200);
CREATE FUNCTION trigger_timing_partition() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO trigger_timing_constraint_audit VALUES (NEW.id); RETURN NEW; END';
CREATE TRIGGER trigger_timing_partition_after AFTER INSERT ON trigger_timing_root
  FOR EACH ROW EXECUTE FUNCTION trigger_timing_partition();
CREATE TABLE trigger_timing_partition_order (position serial, name text);
CREATE FUNCTION trigger_timing_partition_order() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN
     INSERT INTO trigger_timing_partition_order (name) VALUES (TG_NAME);
     RETURN NEW;
   END';
CREATE TRIGGER z_partition_before BEFORE INSERT ON trigger_timing_root
  FOR EACH ROW EXECUTE FUNCTION trigger_timing_partition_order();
CREATE TRIGGER a_partition_before BEFORE INSERT ON trigger_timing_low
  FOR EACH ROW EXECUTE FUNCTION trigger_timing_partition_order();
SELECT c.relname, t.tgparentid = 0, t.tgenabled,
       pg_get_triggerdef(t.oid)
FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid
WHERE t.tgname = 'trigger_timing_partition_after'
ORDER BY c.relname;
SELECT d.deptype, count(*)
FROM pg_depend d JOIN pg_trigger t ON t.oid = d.objid
WHERE t.tgname = 'trigger_timing_partition_after'
GROUP BY d.deptype ORDER BY d.deptype;
ALTER TABLE ONLY trigger_timing_root DISABLE TRIGGER trigger_timing_partition_after;
INSERT INTO trigger_timing_root VALUES (10);
ALTER TABLE trigger_timing_low DISABLE TRIGGER trigger_timing_partition_after;
INSERT INTO trigger_timing_root VALUES (20);
ALTER TABLE trigger_timing_root ENABLE TRIGGER trigger_timing_partition_after;
INSERT INTO trigger_timing_root VALUES (30);
SELECT id FROM trigger_timing_constraint_audit ORDER BY id;
SELECT name FROM trigger_timing_partition_order ORDER BY position;

CREATE FUNCTION trigger_timing_move() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN NEW.id := NEW.id + 100; RETURN NEW; END';
CREATE TRIGGER trigger_timing_move_before BEFORE INSERT ON trigger_timing_root
  FOR EACH ROW EXECUTE FUNCTION trigger_timing_move();
INSERT INTO trigger_timing_root VALUES (40);

ALTER TRIGGER trigger_timing_after ON trigger_timing_target
  RENAME TO trigger_timing_renamed;
SELECT description FROM pg_description d JOIN pg_trigger t ON t.oid = d.objoid
WHERE t.tgname = 'trigger_timing_renamed';
CREATE OR REPLACE TRIGGER trigger_timing_renamed AFTER UPDATE ON trigger_timing_target
  REFERENCING OLD TABLE AS old_set NEW TABLE AS new_set FOR EACH ROW
  EXECUTE PROCEDURE trigger_timing_transition();
SELECT obj_description(oid, 'pg_trigger'), pg_get_triggerdef(oid)
FROM pg_trigger WHERE tgname = 'trigger_timing_renamed';

DROP TABLE trigger_timing_target, trigger_timing_audit,
  trigger_timing_order,
  trigger_timing_constraint_target, trigger_timing_constraint_audit,
  trigger_timing_root, trigger_timing_partition_order CASCADE;
DROP FUNCTION trigger_timing_completed_query(), trigger_timing_constraint(),
  trigger_timing_move(), trigger_timing_partition(),
  trigger_timing_partition_order(), trigger_timing_statement(),
  trigger_timing_transition();
