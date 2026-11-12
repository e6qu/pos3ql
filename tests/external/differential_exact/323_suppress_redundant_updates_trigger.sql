-- suppress_redundant_updates_trigger() skips an UPDATE whose new row is
-- stored identically to the old one, comparing stored values: 1.5 and 1.50
-- differ, as do 'ab' and 'ab ' in varchar. It must be a BEFORE UPDATE row
-- trigger.
CREATE SCHEMA suppress_redundant;
SET search_path = suppress_redundant, public;
CREATE TABLE t(a int, b text, n numeric, f float8);
INSERT INTO t VALUES (1, 'x', 1.0, 0), (2, NULL, 2, 1);
CREATE TRIGGER z BEFORE UPDATE ON t
  FOR EACH ROW EXECUTE FUNCTION suppress_redundant_updates_trigger();
UPDATE t SET a = a;
UPDATE t SET n = 1.00 WHERE a = 1;
UPDATE t SET n = 1.0 WHERE a = 1;
UPDATE t SET f = -0.0 WHERE a = 1;
UPDATE t SET f = '-0'::float8 WHERE a = 1;
UPDATE t SET b = NULL WHERE a = 2;
UPDATE t SET b = 'y' WHERE a = 2 RETURNING *;
SELECT * FROM t ORDER BY a;
-- An AFTER trigger fires only for rows that were not suppressed.
CREATE TABLE log(msg text);
CREATE FUNCTION note() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN INSERT INTO log VALUES (tg_name || ' ' || new.a); RETURN new; END $$;
CREATE TRIGGER after_note AFTER UPDATE ON t FOR EACH ROW EXECUTE FUNCTION note();
UPDATE t SET a = a;
UPDATE t SET a = a, b = 'z';
SELECT * FROM log ORDER BY msg;
SELECT tgname, tgfoid::regproc, tgtype FROM pg_trigger
  WHERE tgrelid = 't'::regclass ORDER BY tgname;
SELECT pg_get_triggerdef(oid) FROM pg_trigger WHERE tgname = 'z';
SELECT count(*) FROM pg_depend
  WHERE classid = 'pg_trigger'::regclass AND refclassid = 'pg_proc'::regclass
    AND refobjid = 'suppress_redundant_updates_trigger'::regproc;
SELECT proname, prorettype::regtype, provolatile, proisstrict, prolang::regproc IS NULL
  FROM pg_proc WHERE oid = 'suppress_redundant_updates_trigger'::regproc;
-- Wrong event, timing or level fails when it fires.
CREATE TRIGGER z_insert BEFORE INSERT ON t
  FOR EACH ROW EXECUTE FUNCTION suppress_redundant_updates_trigger();
INSERT INTO t VALUES (3);
DROP TRIGGER z_insert ON t;
CREATE TRIGGER z_statement BEFORE UPDATE ON t
  FOR EACH STATEMENT EXECUTE FUNCTION suppress_redundant_updates_trigger();
UPDATE t SET a = a;
DROP TRIGGER z_statement ON t;
CREATE TRIGGER z_after AFTER UPDATE ON t
  FOR EACH ROW EXECUTE FUNCTION suppress_redundant_updates_trigger();
UPDATE t SET a = 5 WHERE a = 2;
UPDATE t SET a = a;
DROP TRIGGER z_after ON t;
-- Other column types compare their stored form.
CREATE TABLE j(a jsonb, c int[], d timestamptz, e varchar(3), g bit varying, h numeric);
INSERT INTO j VALUES ('{"a":1}', '{1,2}', '2020-01-01 00:00:00+00', 'ab', B'101', 1.50);
CREATE TRIGGER z BEFORE UPDATE ON j
  FOR EACH ROW EXECUTE FUNCTION suppress_redundant_updates_trigger();
UPDATE j SET a = '{"a": 1}';
UPDATE j SET c = '{1,2}';
UPDATE j SET d = '2020-01-01 05:00:00+05';
UPDATE j SET d = '2020-01-01 06:00:00+05';
UPDATE j SET e = 'ab ';
UPDATE j SET e = 'ab';
UPDATE j SET h = 1.5;
UPDATE j SET g = B'101';
-- Partitions inherit the trigger.
CREATE TABLE p(a int, b text) PARTITION BY RANGE (a);
CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10);
INSERT INTO p VALUES (1, 'x');
CREATE TRIGGER z BEFORE UPDATE ON p
  FOR EACH ROW EXECUTE FUNCTION suppress_redundant_updates_trigger();
UPDATE p SET b = 'x';
UPDATE p SET b = 'y';
SELECT tgrelid::regclass, tgfoid::regproc FROM pg_trigger
  WHERE tgname = 'z' ORDER BY 1::text;
DROP FUNCTION suppress_redundant_updates_trigger();
DROP SCHEMA suppress_redundant CASCADE;
