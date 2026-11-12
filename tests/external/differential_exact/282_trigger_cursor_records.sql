-- A cursor a trigger opens reads NEW and OLD as they were at OPEN, also when
-- the caller fetches it later, and one with data-modifying WITH queries runs
-- them at its first read.
CREATE TABLE t282 (a int);
INSERT INTO t282 VALUES (1), (5);
CREATE TABLE w282 (a int);
CREATE FUNCTION open282() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'c282'; k int := 3;
BEGIN
  OPEN c FOR SELECT a, new.a AS n FROM t282 WHERE a < new.a + k ORDER BY a;
  NEW.a := NEW.a + 100;
  RETURN NEW;
END $$;
CREATE TRIGGER b282 BEFORE INSERT ON t282 FOR EACH ROW EXECUTE FUNCTION open282();
BEGIN;
INSERT INTO t282 VALUES (2);
FETCH ALL FROM c282;
COMMIT;
DROP TRIGGER b282 ON t282;
CREATE FUNCTION write282() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'w282c';
BEGIN
  OPEN c FOR WITH x AS (INSERT INTO w282 VALUES (new.a * 10) RETURNING a) SELECT a FROM x;
  RETURN NEW;
END $$;
CREATE TRIGGER u282 BEFORE UPDATE ON t282 FOR EACH ROW EXECUTE FUNCTION write282();
BEGIN;
UPDATE t282 SET a = 7 WHERE a = 1;
SELECT count(*) FROM w282;
FETCH ALL FROM w282c;
SELECT * FROM w282;
COMMIT;
