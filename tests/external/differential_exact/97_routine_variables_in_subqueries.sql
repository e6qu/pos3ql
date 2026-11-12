-- Routine variables, and a trigger's NEW and OLD rows, read from derived
-- tables, correlated subqueries and set operations inside static PL/pgSQL
-- statements, as the outer level of every subquery in the statement.
CREATE SCHEMA routine_scopes;
SET search_path = routine_scopes, public;
CREATE TABLE t(a int, b text);
INSERT INTO t VALUES (1, 'one'), (2, 'two');
CREATE FUNCTION writes(x int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE n int;
BEGIN
  INSERT INTO t SELECT s.v, 'ins' FROM (SELECT x AS v) s;
  UPDATE t SET b = 'upd' FROM (SELECT x AS v) s WHERE t.a = s.v;
  MERGE INTO t USING (SELECT x AS v) s ON t.a = s.v
    WHEN MATCHED THEN UPDATE SET b = b || '+merged';
  DELETE FROM t USING (SELECT x AS v) s WHERE t.a = s.v + 100;
  INSERT INTO t VALUES ((SELECT y FROM (SELECT x + 1 AS y) d), 'sub');
  UPDATE t SET b = d.z FROM (SELECT x + 1 AS y, 'derived' AS z) d WHERE a = d.y;
  SELECT count(*) INTO n FROM (SELECT x AS v) s JOIN t ON t.a = s.v;
  RETURN n;
END$$;
SELECT writes(1);
SELECT * FROM t ORDER BY a, b;
CREATE FUNCTION reads(x int, q int) RETURNS SETOF int LANGUAGE plpgsql AS $$
DECLARE r record; c refcursor; v int;
BEGIN
  IF q = 1 THEN
    FOR r IN SELECT * FROM (SELECT x + 1 AS y) d LOOP RETURN NEXT r.y; END LOOP;
    RETURN QUERY SELECT y FROM (VALUES (x + 3)) d(y);
    RETURN QUERY SELECT y FROM LATERAL (SELECT x + 5 AS y) d;
    RETURN QUERY SELECT (SELECT y FROM (SELECT x + 6 AS y) d);
    RETURN QUERY SELECT max(y) FROM (SELECT x AS y UNION ALL SELECT x + 7) d;
    OPEN c FOR SELECT * FROM (SELECT x + 8) d;
    FETCH c INTO v;
    CLOSE c;
    RETURN NEXT v;
  ELSIF q = 2 THEN
    RETURN QUERY SELECT (SELECT t.a + x) FROM t ORDER BY 1;
    RETURN QUERY SELECT (SELECT t.a + x) FROM t;
    RETURN QUERY SELECT (SELECT y FROM (SELECT t.a + x AS y) d) FROM t;
    RETURN QUERY SELECT d.y FROM t, LATERAL (SELECT t.a + x AS y) d ORDER BY 1;
  ELSE
    RETURN QUERY SELECT (SELECT t.a + x) FROM t GROUP BY a ORDER BY 1;
    RETURN QUERY SELECT (SELECT count(*)::int + x) FROM t GROUP BY a ORDER BY 1;
    RETURN QUERY SELECT (SELECT t.a + x) + count(*) OVER ()::int FROM t ORDER BY 1;
    RETURN QUERY SELECT a FROM t
      WHERE EXISTS (SELECT 1 FROM (SELECT t.a + x AS y) d WHERE y > 11) ORDER BY 1;
    RETURN QUERY SELECT sum(a)::int FROM t
      HAVING sum(a) > (SELECT y FROM (SELECT x AS y) d);
    RETURN QUERY SELECT a FROM t
      WHERE a IN (SELECT y FROM (SELECT x - 8 AS y UNION ALL SELECT x - 9) d) ORDER BY 1;
  END IF;
END$$;
SELECT * FROM reads(10, 1);
SELECT * FROM reads(10, 2);
SELECT * FROM reads(10, 3);
-- A correlated derived table in plain SQL reads the enclosing row.
SELECT (SELECT y FROM (SELECT t.a AS y) d) FROM t ORDER BY 1;
SELECT a FROM t WHERE a IN (SELECT t.a UNION SELECT 0) ORDER BY 1;
DO $$DECLARE x int := 3; n int;
BEGIN SELECT y INTO n FROM (SELECT x * 2 AS y) d; RAISE NOTICE 'n %', n; END$$;
CREATE PROCEDURE p(x int) LANGUAGE plpgsql AS $$
BEGIN INSERT INTO t SELECT y, 'proc' FROM (SELECT x AS y) d; END$$;
CALL p(77);
CREATE FUNCTION sqlf(x int) RETURNS int LANGUAGE sql AS $$ SELECT y FROM (SELECT x + 1 AS y) d $$;
SELECT sqlf(5);
-- NEW, OLD and a record holding NEW inside a trigger's statements.
CREATE TABLE tt(id int, v int);
INSERT INTO tt VALUES (1, 10);
CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record; n int;
BEGIN
  SELECT count(*) INTO n FROM t WHERE a = (new).id;
  UPDATE t SET b = 'x' || (new).v WHERE a = (new).id;
  INSERT INTO t VALUES ((new).id + 10, (old).v::text);
  r := new;
  RAISE NOTICE 'r % % %', n, (r).v, r.v;
  INSERT INTO t SELECT d.i, 'trg' FROM (SELECT (new).id + 50 AS i) d;
  INSERT INTO t SELECT d.i, 'trg2' FROM (SELECT new.id + 60 AS i) d;
  INSERT INTO t SELECT d.i, 'trg3' FROM (SELECT (r).id + 70 AS i) d;
  INSERT INTO t SELECT d.i, 'trg4' FROM (SELECT r.id + 80 AS i) d;
  RETURN new;
END$$;
CREATE TRIGGER trg BEFORE UPDATE ON tt FOR EACH ROW EXECUTE FUNCTION trg();
UPDATE tt SET v = 20;
SELECT * FROM t ORDER BY a, b;
-- A derived table's own WITH, and an INSERT source's, inside a routine.
CREATE FUNCTION with_sources(x int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE n int;
BEGIN
  INSERT INTO t SELECT * FROM (WITH c AS (SELECT x AS y) SELECT y, 'cte' FROM c) d;
  INSERT INTO t WITH c AS (SELECT 300 AS y) SELECT y, 'cte2' FROM c;
  SELECT y INTO n FROM (WITH c AS (SELECT x + 1 AS y) SELECT y FROM c) d;
  IF EXISTS (SELECT 1 FROM (SELECT x AS y) d WHERE y = 200) THEN
    n := n + (SELECT y FROM (SELECT x + 1 AS y) d);
  END IF;
  RETURN n;
END$$;
SELECT with_sources(200);
SELECT * FROM t WHERE a >= 200 ORDER BY a, b;
RESET search_path;
DROP SCHEMA routine_scopes CASCADE;
