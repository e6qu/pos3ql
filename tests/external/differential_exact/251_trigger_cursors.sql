-- Triggers fired by ordinary statements open, fetch, move and close SQL
-- cursors, in BEFORE, AFTER and statement triggers, and from nested writes.
CREATE TABLE t251 (a int, b text);
CREATE TABLE u251 (v int);
INSERT INTO u251 VALUES (10), (20), (30);
CREATE FUNCTION trg251() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE c refcursor; r record; s int := 0; x int; y int;
BEGIN
  OPEN c FOR SELECT v FROM u251 ORDER BY v;
  LOOP FETCH c INTO r; EXIT WHEN NOT FOUND; s := s + r.v; END LOOP;
  CLOSE c;
  OPEN c SCROLL FOR SELECT v FROM u251 ORDER BY v;
  FETCH LAST FROM c INTO x;
  FETCH PRIOR FROM c INTO y;
  MOVE FIRST IN c;
  RAISE NOTICE '% % %: sum % last % prior %', TG_WHEN, TG_LEVEL, TG_OP, s, x, y;
  CLOSE c;
  OPEN c FOR EXECUTE 'SELECT $1 * v FROM u251 WHERE v > $2 ORDER BY v' USING 2, 10;
  FETCH c INTO x;
  RAISE NOTICE 'dynamic %', x;
  CLOSE c;
  IF TG_LEVEL = 'ROW' AND TG_OP <> 'DELETE' THEN
    OPEN c FOR SELECT max(v) FROM u251;
    FETCH c INTO NEW.a;
    CLOSE c;
    RETURN NEW;
  END IF;
  RETURN OLD;
END $$;
CREATE TRIGGER tb251 BEFORE INSERT OR UPDATE ON t251 FOR EACH ROW EXECUTE FUNCTION trg251();
CREATE TRIGGER ta251 AFTER INSERT ON t251 FOR EACH ROW EXECUTE FUNCTION trg251();
CREATE TRIGGER ts251 BEFORE DELETE ON t251 FOR EACH STATEMENT EXECUTE FUNCTION trg251();
INSERT INTO t251 VALUES (1, 'x');
UPDATE t251 SET b = 'y';
DO $$ BEGIN INSERT INTO t251 VALUES (2, 'do'); END $$;
CREATE FUNCTION ins251() RETURNS void LANGUAGE plpgsql AS $$ BEGIN INSERT INTO t251 VALUES (3, 'f'); END $$;
SELECT ins251();
WITH w AS (INSERT INTO t251 VALUES (4, 'cte') RETURNING b) SELECT * FROM w;
DELETE FROM t251 WHERE b = 'cte';
SELECT * FROM t251 ORDER BY b;
-- A cursor a trigger leaves open belongs to the session until the transaction ends.
CREATE FUNCTION keep251() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'kept251';
BEGIN OPEN c FOR SELECT v * 2 AS w FROM u251 ORDER BY v; RETURN NEW; END $$;
CREATE TRIGGER keep251 AFTER INSERT ON u251 FOR EACH ROW EXECUTE FUNCTION keep251();
BEGIN;
INSERT INTO u251 VALUES (40);
SELECT name, is_holdable FROM pg_cursors;
FETCH ALL FROM kept251;
COMMIT;
SELECT count(*) FROM pg_cursors;
-- A second open of a cursor still in use fails before its query runs.
BEGIN;
DECLARE kept251 CURSOR FOR SELECT 1;
INSERT INTO u251 VALUES (50);
ROLLBACK;
DO $$ DECLARE c refcursor := 'dup251'; BEGIN OPEN c FOR SELECT 1; OPEN c FOR SELECT 2; END $$;
DROP TRIGGER keep251 ON u251;
-- A cursor declared by the session can be read by a trigger.
CREATE FUNCTION outer251() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'outer251'; x int;
BEGIN FETCH c INTO x; RAISE NOTICE 'outer %', x; RETURN NEW; END $$;
CREATE TRIGGER outer251 BEFORE INSERT ON u251 FOR EACH ROW EXECUTE FUNCTION outer251();
BEGIN;
DECLARE outer251 CURSOR FOR SELECT v FROM u251 ORDER BY v DESC;
INSERT INTO u251 VALUES (60);
FETCH outer251;
COMMIT;
