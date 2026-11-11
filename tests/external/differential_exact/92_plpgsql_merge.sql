-- PL/pgSQL runs MERGE as a static statement that reads the routine's
-- variables and parameters, and through EXECUTE with USING values; both set
-- ROW_COUNT and FOUND, and routine calls inside see earlier changed rows.
CREATE SCHEMA plpgsql_merge;
SET search_path = plpgsql_merge, public;
CREATE TABLE items(id int, a bigint);
CREATE TABLE source(id int);
INSERT INTO items VALUES (1, 1), (2, 1);
INSERT INTO source VALUES (1), (3);

DO $$
DECLARE n int; v int := 9;
BEGIN
  MERGE INTO items t USING source s ON t.id = s.id
    WHEN MATCHED THEN UPDATE SET a = v + t.a
    WHEN NOT MATCHED THEN INSERT VALUES (s.id, v);
  GET DIAGNOSTICS n = ROW_COUNT;
  RAISE NOTICE 'static rows % found %', n, FOUND;
  MERGE INTO items t USING source s ON t.id = s.id + 100
    WHEN MATCHED THEN DELETE;
  RAISE NOTICE 'no match found %', FOUND;
END$$;
SELECT * FROM items ORDER BY id;

CREATE FUNCTION upsert(key int, amount bigint) RETURNS int LANGUAGE plpgsql AS $$
DECLARE n int;
BEGIN
  MERGE INTO items t USING (VALUES (1)) s(x) ON t.id = key
    WHEN MATCHED AND amount = 0 THEN DELETE
    WHEN MATCHED THEN UPDATE SET a = t.a + amount
    WHEN NOT MATCHED THEN INSERT VALUES (key, amount);
  GET DIAGNOSTICS n = ROW_COUNT;
  RETURN n;
END$$;
SELECT upsert(2, 5), upsert(7, 3), upsert(1, 0);
SELECT * FROM items ORDER BY id;

DO $$
DECLARE n int; action text; key int;
BEGIN
  EXECUTE 'MERGE INTO items t USING source s ON t.id = s.id
           WHEN MATCHED THEN UPDATE SET a = $1
           WHEN NOT MATCHED THEN INSERT VALUES (s.id, $1)' USING 4;
  GET DIAGNOSTICS n = ROW_COUNT;
  RAISE NOTICE 'dynamic rows %', n;
  EXECUTE 'MERGE INTO items t USING (VALUES ($1)) s(id) ON t.id = s.id
           WHEN MATCHED THEN DELETE RETURNING merge_action(), t.id'
    INTO action, key USING 7;
  RAISE NOTICE 'dynamic returning % %', action, key;
END$$;
SELECT * FROM items ORDER BY id;

-- Calls inside a procedural MERGE see the rows it changed before them.
CREATE FUNCTION total() RETURNS bigint LANGUAGE plpgsql
  AS $$BEGIN RETURN (SELECT sum(a) FROM items); END$$;
DO $$BEGIN
  MERGE INTO items t USING source s ON t.id = s.id
    WHEN MATCHED THEN UPDATE SET a = total()
    WHEN NOT MATCHED THEN INSERT VALUES (s.id, total());
  EXECUTE 'MERGE INTO items t USING source s ON t.id = s.id
           WHEN MATCHED THEN UPDATE SET a = total()';
END$$;
SELECT * FROM items ORDER BY id;

DROP SCHEMA plpgsql_merge CASCADE;
