-- FOR loops over INSERT, UPDATE, DELETE and MERGE with RETURNING, static and
-- through EXECUTE: the statement runs to completion before the first
-- iteration, and routine calls in its rows see the rows changed before them.
CREATE SCHEMA for_changes;
SET search_path = for_changes, public;
CREATE TABLE t(id int PRIMARY KEY, a int);
INSERT INTO t VALUES (1, 10), (2, 20);
CREATE FUNCTION total() RETURNS bigint LANGUAGE plpgsql
  AS $$BEGIN RETURN (SELECT sum(a) FROM t); END$$;
CREATE FUNCTION f(x int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE r record; s int := 0; n int; v int;
BEGIN
  FOR r IN INSERT INTO t VALUES (x, x * 10), (x + 1, x * 20) RETURNING * LOOP
    s := s + r.a; RAISE NOTICE 'ins % %', r.id, r.a;
  END LOOP;
  RAISE NOTICE 'found %', found;
  FOR r IN UPDATE t SET a = a + 1 WHERE id >= x RETURNING id, a LOOP
    s := s + r.a;
  END LOOP;
  GET DIAGNOSTICS n = ROW_COUNT;
  RAISE NOTICE 'rows %', n;
  FOR r IN DELETE FROM t WHERE id = x RETURNING id LOOP
    s := s + r.id; EXIT;
  END LOOP;
  FOR r IN MERGE INTO t USING (VALUES (1)) v(id) ON t.id = v.id
      WHEN MATCHED THEN UPDATE SET a = 0 RETURNING t.* LOOP
    s := s + r.id;
  END LOOP;
  FOR r IN EXECUTE 'INSERT INTO t VALUES ($1, 0) RETURNING id' USING x + 50 LOOP
    s := s + r.id;
  END LOOP;
  FOR v IN INSERT INTO t VALUES (x + 60, 7) RETURNING a, id LOOP
    RAISE NOTICE 'first column %', v;
  END LOOP;
  FOR v IN EXECUTE 'DELETE FROM t WHERE id = $1 RETURNING id' USING x + 60 LOOP
    RAISE NOTICE 'deleted %', v;
  END LOOP;
  FOR r IN DELETE FROM t WHERE false RETURNING id LOOP END LOOP;
  RAISE NOTICE 'found %', found;
  RETURN s;
END$$;
SELECT f(5);
SELECT * FROM t ORDER BY id;

-- Routine calls in the rows and RETURNING list see earlier rows' changes.
DO $$DECLARE r record;
BEGIN
  FOR r IN UPDATE t SET a = total() WHERE id < 3 RETURNING id, a, total() AS now LOOP
    RAISE NOTICE '% % %', r.id, r.a, r.now;
  END LOOP;
  FOR r IN EXECUTE 'INSERT INTO t VALUES (70, 1), (71, total()::int) RETURNING id, a'
  LOOP
    RAISE NOTICE '% %', r.id, r.a;
  END LOOP;
END$$;
SELECT * FROM t ORDER BY id;

-- A data change without RETURNING cannot feed a loop.
DO $$DECLARE r record; BEGIN FOR r IN INSERT INTO t VALUES (80, 0) LOOP END LOOP; END$$;
DO $$DECLARE r record; BEGIN FOR r IN DELETE FROM t LOOP END LOOP; END$$;
SELECT count(*) FROM t;
DROP SCHEMA for_changes CASCADE;
