-- UPDATE and DELETE qualify each row just before changing it, as
-- PostgreSQL's executor pulls a row from the scan and then changes it:
-- notices from a condition's routine calls interleave with the row's SET
-- expressions, triggers and RETURNING list, and a condition that reads the
-- target sees the rows the statement already changed. A routine call stops
-- the write and resumes it at the unfinished row; rows already changed, and
-- rows whose condition failed, are not qualified again.
CREATE FUNCTION q490(tag text, x int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE '% %', tag, x; RETURN x; END $$;
CREATE TABLE t490 (id int PRIMARY KEY, v int);
INSERT INTO t490 SELECT g, g * 10 FROM generate_series(1, 4) g;
UPDATE t490 SET v = q490('set', v) WHERE q490('where', id) % 2 = 0;
DELETE FROM t490 WHERE q490('delete where', id) > 2 RETURNING q490('returning', id);
INSERT INTO t490 VALUES (3, 30), (4, 40);
CREATE FUNCTION t490_before() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'before % %', TG_OP, OLD.id; RETURN COALESCE(NEW, OLD); END $$;
CREATE TRIGGER t490_before BEFORE UPDATE OR DELETE ON t490
  FOR EACH ROW EXECUTE FUNCTION t490_before();
UPDATE t490 SET v = v + 1 WHERE q490('w', id) > 0;
-- An index answers part of the condition; the rest runs for its rows.
UPDATE t490 SET v = 5 WHERE id = 1 AND q490('indexed', id) > 0;
UPDATE t490 SET v = v WHERE q490('uw', id) IN (2, 3) RETURNING q490('ur', id);
-- A condition that reads the target sees the earlier rows' changes.
CREATE FUNCTION t490_sum() RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE s bigint;
BEGIN SELECT sum(v) INTO s FROM t490; RAISE NOTICE 'sum %', s; RETURN s; END $$;
UPDATE t490 SET v = v + 1 WHERE t490_sum() > 0;
CREATE SEQUENCE s490;
UPDATE t490 SET v = nextval('s490') WHERE nextval('s490') % 2 = 0 RETURNING id, v;
-- A condition that fails part-way stops the statement.
UPDATE t490 SET v = q490('fset', id) WHERE 10 / (q490('fw', id) - 3) <> 0;
SELECT * FROM t490 ORDER BY id;

-- UPDATE ... FROM and DELETE ... USING join each target row as it changes.
CREATE TABLE f490 (id int, w int);
INSERT INTO f490 VALUES (1, 100), (2, 200);
UPDATE t490 SET v = f490.w FROM f490
 WHERE f490.id = t490.id AND q490('join', f490.w) > 0;
DELETE FROM t490 USING f490
 WHERE f490.id = t490.id AND q490('using', f490.w) > 100
 RETURNING q490('using returning', f490.w);
SELECT * FROM t490 ORDER BY id;

-- Row-security conditions run before the statement's own, and a policy
-- the SELECT and UPDATE commands share runs once per row.
CREATE TABLE r490 (id int PRIMARY KEY, v int);
INSERT INTO r490 SELECT g, g FROM generate_series(1, 4) g;
CREATE ROLE r490_user;
GRANT ALL ON r490 TO r490_user;
ALTER TABLE r490 ENABLE ROW LEVEL SECURITY;
CREATE POLICY r490_p ON r490 USING (q490('policy', id) > 1);
SET ROLE r490_user;
UPDATE r490 SET v = q490('policy set', v) WHERE q490('policy where', id) < 4;
DELETE FROM r490 WHERE q490('policy delete', id) = 3;
RESET ROLE;
DROP POLICY r490_p ON r490;
DROP TABLE r490;
DROP OWNED BY r490_user;
DROP ROLE r490_user;

-- A write inside a routine body, and one a statement trigger runs over its
-- transition table, qualify their rows the same way.
CREATE TABLE n490 (id int, n int);
INSERT INTO n490 VALUES (1, 0), (2, 0), (3, 0);
CREATE FUNCTION n490_inner() RETURNS void LANGUAGE plpgsql AS $$
BEGIN UPDATE n490 SET n = q490('inner set', n + 1) WHERE q490('inner where', id) > 1; END $$;
SELECT n490_inner();
CREATE FUNCTION t490_after() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  UPDATE n490 SET n = q490('transition set', n + 1)
   WHERE q490('transition where', n490.id) > 0 AND n490.id IN (SELECT id FROM nt);
  DELETE FROM n490 USING nt WHERE nt.id = n490.id AND q490('transition delete', nt.id) = 3;
  RETURN NULL;
END $$;
CREATE TRIGGER t490_after AFTER UPDATE ON t490 REFERENCING NEW TABLE AS nt
  FOR EACH STATEMENT EXECUTE FUNCTION t490_after();
UPDATE t490 SET v = v + 1 WHERE id IN (1, 3);
SELECT * FROM n490 ORDER BY id;

-- An uncorrelated subquery runs once, at the first row that reads it, also
-- when a later row stops the statement for a routine call.
CREATE TABLE u490 (id int PRIMARY KEY, v int);
INSERT INTO u490 VALUES (1, 0), (2, 0), (3, 0);
UPDATE u490 SET v = 1
 WHERE id IN (SELECT g FROM generate_series(2, 3) g) AND q490('after subquery', id) > 0;
UPDATE u490 SET v = q490('s', id) + (SELECT max(q490('once', g)) FROM generate_series(1, 2) g);
UPDATE u490 SET v = CASE WHEN id IN (SELECT g FROM generate_series(1, 3) g)
                         THEN q490('case', id) END;
UPDATE u490 SET v = 0 WHERE id IN (SELECT q490('sub', g) FROM generate_series(2, 3) g);
SELECT * FROM u490 ORDER BY id;

-- A row changed by a BEFORE trigger's own write, or by the condition's
-- routine, is reported when the statement reaches it.
CREATE TABLE b490 (id int PRIMARY KEY, v int);
INSERT INTO b490 VALUES (1, 0), (2, 0), (3, 0);
CREATE FUNCTION b490_before() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF OLD.id = 1 THEN UPDATE b490 SET v = 99 WHERE id = 3; END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER b490_before BEFORE UPDATE ON b490 FOR EACH ROW EXECUTE FUNCTION b490_before();
UPDATE b490 SET v = v + 1 WHERE q490('b', id) > 0;
DROP TRIGGER b490_before ON b490;
CREATE FUNCTION b490_bump(x int) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN IF x = 1 THEN UPDATE b490 SET v = 100 WHERE id = 3; END IF; RETURN true; END $$;
UPDATE b490 SET v = v + 1 WHERE b490_bump(id);
DELETE FROM b490 WHERE b490_bump(id);
SELECT * FROM b490 ORDER BY id;

-- A routine call moves a row to a partition that does not exist.
CREATE TABLE p490 (id int, v int) PARTITION BY RANGE (id);
CREATE TABLE p490_1 PARTITION OF p490 FOR VALUES FROM (0) TO (10);
CREATE TABLE p490_2 PARTITION OF p490 FOR VALUES FROM (10) TO (20);
INSERT INTO p490 VALUES (1, 0), (2, 0), (11, 0), (12, 0);
UPDATE p490 SET id = q490('move', id + 10) WHERE q490('partition where', id) IN (1, 11, 12);
INSERT INTO p490 VALUES (25, 0);
DELETE FROM p490 WHERE q490('partition delete', id) > 11;
SELECT tableoid::regclass, * FROM p490 ORDER BY id;

-- A prepared write qualifies its rows the same way on each execution.
PREPARE w490(int) AS UPDATE u490 SET v = q490('prepared set', v) WHERE q490('prepared where', id) = $1;
EXECUTE w490(2);
EXECUTE w490(3);
DEALLOCATE w490;
