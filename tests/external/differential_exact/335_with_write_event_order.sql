-- Data-modifying WITH queries queue their AFTER events into the whole
-- statement's events: a query nothing reads runs after the main statement,
-- last written first, and a read one fires with the statement's own events.
CREATE TABLE eo335 (a int);
CREATE TABLE eo335_other (a int);
CREATE FUNCTION eo335_note() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE '% % % % %', TG_TABLE_NAME, TG_WHEN, TG_LEVEL, TG_OP,
    CASE WHEN TG_LEVEL = 'ROW' THEN coalesce(NEW, OLD)::text END;
  RETURN coalesce(NEW, OLD);
END $$;
CREATE FUNCTION eo335_value(v int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'value %', v; RETURN v; END $$;
CREATE TRIGGER a AFTER INSERT OR UPDATE OR DELETE ON eo335 FOR EACH ROW EXECUTE FUNCTION eo335_note();
CREATE TRIGGER s AFTER INSERT OR UPDATE OR DELETE ON eo335 FOR EACH STATEMENT EXECUTE FUNCTION eo335_note();
CREATE TRIGGER a AFTER INSERT OR UPDATE OR DELETE ON eo335_other FOR EACH ROW EXECUTE FUNCTION eo335_note();
CREATE TRIGGER s AFTER INSERT OR UPDATE OR DELETE ON eo335_other FOR EACH STATEMENT EXECUTE FUNCTION eo335_note();
WITH w AS (INSERT INTO eo335 VALUES (1), (2)) INSERT INTO eo335_other VALUES (10), (20);
WITH w AS (INSERT INTO eo335 VALUES (3)),
     x AS (INSERT INTO eo335_other VALUES (30))
SELECT eo335_value(0);
WITH w AS (UPDATE eo335 SET a = a + 100 WHERE a < 3 RETURNING a)
SELECT eo335_value(count(*)::int) FROM w;
WITH w AS (DELETE FROM eo335_other WHERE a = 10),
     x AS (UPDATE eo335 SET a = -a WHERE a = 3)
UPDATE eo335_other SET a = a + 1 WHERE a = 20;
WITH w AS (DELETE FROM eo335 RETURNING a) SELECT a FROM w ORDER BY a;
SELECT * FROM eo335_other ORDER BY a;
DROP TABLE eo335, eo335_other;
DROP FUNCTION eo335_note(), eo335_value(int);
