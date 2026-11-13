-- A trigger WHEN condition is analysed in the statement's text: a missing
-- column, an unknown literal's conversion and a BEFORE trigger's reference to
-- a generated NEW column point at their place in the condition.
CREATE TABLE t367 (a int, b int GENERATED ALWAYS AS (a * 2));
CREATE FUNCTION f367() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN new; END $$;
CREATE TRIGGER x367 BEFORE UPDATE ON t367 FOR EACH ROW WHEN (new.zz > 0) EXECUTE FUNCTION f367();
CREATE TRIGGER x367 BEFORE UPDATE ON t367 FOR EACH ROW WHEN (new.a > 'x') EXECUTE FUNCTION f367();
CREATE TRIGGER x367 BEFORE UPDATE ON t367 FOR EACH ROW WHEN (new.b > 0) EXECUTE FUNCTION f367();
CREATE TRIGGER x367 AFTER UPDATE ON t367 FOR EACH ROW WHEN (new.b > old.b) EXECUTE FUNCTION f367();
SELECT tgname, pg_get_triggerdef(oid) FROM pg_trigger WHERE tgrelid = 't367'::regclass;
DROP TABLE t367;
DROP FUNCTION f367();
