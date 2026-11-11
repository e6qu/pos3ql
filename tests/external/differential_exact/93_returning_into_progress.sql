-- Static PL/pgSQL INSERT, UPDATE, DELETE and MERGE with RETURNING ... INTO:
-- routine calls in their rows and RETURNING lists see the rows the
-- statement already changed, and INTO receives the one returned row (or
-- reports PostgreSQL's errors for none or several).
CREATE SCHEMA returning_into;
SET search_path = returning_into, public;
CREATE TABLE items(id int, a bigint);
CREATE TABLE calls(label text, seen bigint);
CREATE FUNCTION seen(label text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE total bigint;
BEGIN
  SELECT coalesce(sum(a), 0) + count(*) INTO total FROM items;
  RAISE NOTICE '% saw %', label, total;
  INSERT INTO calls VALUES (label, total);
  RETURN total;
END$$;
INSERT INTO items VALUES (1, 1), (2, 1), (3, 1);

-- Each row's call sees the rows updated before it; INTO takes one row.
DO $$DECLARE x bigint; y bigint;
BEGIN
  UPDATE items SET a = seen('update') WHERE id = 1 RETURNING a INTO x;
  RAISE NOTICE 'update returned %', x;
  INSERT INTO items VALUES (4, seen('insert')) RETURNING id, a INTO x, y;
  RAISE NOTICE 'insert returned % %', x, y;
  DELETE FROM items WHERE id = 4 RETURNING seen('delete returning') INTO x;
  RAISE NOTICE 'delete returned % found %', x, found;
END$$;
SELECT * FROM items ORDER BY id;
SELECT label, seen FROM calls ORDER BY label, seen;
TRUNCATE calls;

-- A data change's INTO takes at most one row even without STRICT. Each
-- call (its notice shows what it saw) sees the rows changed before it; the
-- error then rolls the statement back.
DO $$DECLARE x bigint;
BEGIN
  UPDATE items SET a = seen('many') RETURNING a INTO x;
END$$;
DO $$DECLARE x bigint;
BEGIN
  BEGIN
    INSERT INTO items SELECT g, seen('insert many') FROM generate_series(5, 6) g
      RETURNING a INTO x;
  EXCEPTION WHEN too_many_rows THEN
    RAISE NOTICE 'caught %: x is %', sqlerrm, x;
  END;
  DELETE FROM items WHERE id = seen('delete key') - 7 RETURNING id INTO x;
  RAISE NOTICE 'deleted %', x;
  RAISE EXCEPTION 'undo';
END$$;
DO $$DECLARE x bigint;
BEGIN
  UPDATE items SET a = seen('none') WHERE id > 100 RETURNING a INTO STRICT x;
END$$;
DO $$DECLARE x bigint;
BEGIN
  UPDATE items SET a = seen('none') WHERE id > 100 RETURNING a INTO x;
  RAISE NOTICE 'no row: % found %', x, found;
END$$;
SELECT * FROM items ORDER BY id;
SELECT label, seen FROM calls ORDER BY label, seen;
TRUNCATE calls;

-- A record target receives the RETURNING row's fields.
CREATE FUNCTION bump(key int) RETURNS text LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
  UPDATE items SET a = a + seen('record') WHERE id = key RETURNING * INTO r;
  RETURN r.id || ':' || r.a;
END$$;
SELECT bump(2), bump(3);
SELECT * FROM items ORDER BY id;
SELECT label, seen FROM calls ORDER BY label, seen;
TRUNCATE calls;

-- MERGE ... RETURNING ... INTO, with calls in its actions and RETURNING list.
CREATE TABLE source(id int, a bigint);
INSERT INTO source VALUES (2, 0), (9, 0);
DO $$DECLARE action text; key int; total bigint;
BEGIN
  MERGE INTO items t USING (SELECT * FROM source WHERE id = 9) s ON t.id = s.id
    WHEN MATCHED THEN UPDATE SET a = seen('merge update')
    WHEN NOT MATCHED THEN INSERT VALUES (s.id, seen('merge insert'))
    RETURNING merge_action(), t.id, seen('merge returning') INTO action, key, total;
  RAISE NOTICE 'merge returned % % %', action, key, total;
  MERGE INTO items t USING source s ON t.id = s.id
    WHEN MATCHED THEN UPDATE SET a = seen('merge many')
    RETURNING t.id INTO key;
END$$;
SELECT * FROM items ORDER BY id;
SELECT label, seen FROM calls ORDER BY label, seen;
TRUNCATE calls;

-- RETURNING without INTO, and INTO without RETURNING, are PL/pgSQL errors.
DO $$BEGIN INSERT INTO items VALUES (5, 0) RETURNING id; END$$;
DO $$DECLARE x int; BEGIN DELETE FROM items WHERE id = 1 INTO x; END$$;
SELECT * FROM items ORDER BY id;

DROP SCHEMA returning_into CASCADE;
