-- UPDATE ... FROM and DELETE ... USING change their target rows in the
-- order PostgreSQL's join returns them, each on its first join row with
-- that row's FROM row: row triggers, RETURNING and routine callers see
-- that order, also when the FROM table is the join's outer relation.
CREATE TABLE ta672 (id int, v int, name text);
INSERT INTO ta672 SELECT g, g * 10, 'n' || g FROM generate_series(1, 300) g;
CREATE TABLE sb672 (k bigint, w int, name text);
INSERT INTO sb672 SELECT g % 50, g, 'n' || (g % 50) FROM generate_series(1, 120) g;
CREATE TABLE tiny672 (k int, w int);
INSERT INTO tiny672 VALUES (3, 1), (3, 2), (5, 3);
ANALYZE ta672;
ANALYZE sb672;
ANALYZE tiny672;
CREATE TABLE log672 (n serial, op text, id int, v int);
CREATE FUNCTION trg672() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO log672 (op, id, v) VALUES (TG_OP, OLD.id, NEW.v);
  RETURN NEW;
END $$;
CREATE TRIGGER ta672_upd BEFORE UPDATE ON ta672 FOR EACH ROW EXECUTE FUNCTION trg672();
EXPLAIN (COSTS OFF) UPDATE ta672 SET v = tiny672.w FROM tiny672 WHERE ta672.id = tiny672.k;
UPDATE ta672 SET v = tiny672.w FROM tiny672 WHERE ta672.id = tiny672.k RETURNING ta672.id, ta672.v, tiny672.*;
UPDATE tiny672 SET w = ta672.v FROM ta672 WHERE ta672.id = tiny672.k RETURNING tiny672.*, ta672.id;
UPDATE ta672 AS a SET v = b.w FROM sb672 AS b WHERE a.id = b.k AND a.id BETWEEN 1 AND 6 RETURNING a.id, a.v, b.w;
UPDATE ta672 SET v = sb672.w FROM sb672 WHERE ta672.name = sb672.name AND sb672.w > 100 RETURNING ta672.id, sb672.w;
UPDATE ta672 SET v = 0 FROM tiny672 WHERE ta672.v < tiny672.w * 10 RETURNING ta672.id, tiny672.w;
SELECT op, id, v FROM log672 ORDER BY n;
DO $$
DECLARE r record;
BEGIN
  FOR r IN UPDATE ta672 SET v = sb672.w + 1 FROM sb672 WHERE ta672.id = sb672.k AND ta672.id < 4
    RETURNING ta672.id, ta672.v
  LOOP
    RAISE NOTICE '% %', r.id, r.v;
  END LOOP;
END $$;
DELETE FROM ta672 USING tiny672 WHERE ta672.v < tiny672.k RETURNING ta672.id, tiny672.*;
SELECT count(*), sum(v) FROM ta672;
