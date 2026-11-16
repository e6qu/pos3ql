-- A cursor declared in a nested block's DECLARE section is a bound
-- `refcursor` variable of that block: its statements and handlers open,
-- fetch and loop over it, it hides an enclosing cursor of the same name,
-- reads the variables declared before it, and starts closed (NULL) each
-- time the block is entered.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/pl/plpgsql/src/pl_gram.y
CREATE TABLE t744 (id integer, name text);
INSERT INTO t744 VALUES (1, 'a'), (2, 'b'), (3, 'c');
DO $$ BEGIN DECLARE c CURSOR FOR SELECT 1 AS x; r record; BEGIN OPEN c; FETCH c INTO r; RAISE NOTICE '%', r; CLOSE c; END; END $$;
DO $$ DECLARE v integer := 4; BEGIN DECLARE c CURSOR (k integer) FOR SELECT k + v AS x; r record; BEGIN FOR r IN c(3) LOOP RAISE NOTICE '%', r; END LOOP; END; END $$;
DO $$ BEGIN DECLARE c CURSOR FOR SELECT g FROM generate_series(1, 3) g; BEGIN OPEN c; END; DECLARE c CURSOR FOR SELECT 9; r record; BEGIN OPEN c; FETCH c INTO r; RAISE NOTICE '%', r; END; END $$;
DO $$ DECLARE c CURSOR FOR SELECT 'outer' AS w; r record; BEGIN
  DECLARE c CURSOR FOR SELECT 'inner' AS w; BEGIN OPEN c; FETCH c INTO r; RAISE NOTICE '%', r; CLOSE c; END;
  OPEN c; FETCH c INTO r; RAISE NOTICE '%', r;
END $$;
DO $$ DECLARE r record; BEGIN
  FOR i IN 1..3 LOOP
    DECLARE c CURSOR FOR SELECT name FROM t744 WHERE id = i; BEGIN
      RAISE NOTICE 'entered %', c IS NULL;
      OPEN c; FETCH c INTO r; RAISE NOTICE '% %', i, r.name;
    END;
  END LOOP;
END $$;
DO $$ BEGIN <<blk>> DECLARE c CURSOR FOR SELECT id FROM t744 ORDER BY id DESC; n integer; BEGIN
  OPEN blk.c; MOVE c; FETCH c INTO n; RAISE NOTICE '%', n; CLOSE blk.c;
END; END $$;
DO $$ BEGIN DECLARE c SCROLL CURSOR FOR SELECT id FROM t744 ORDER BY id; n integer; BEGIN
  c := 'scroll744'; OPEN c; FETCH LAST FROM c INTO n; RAISE NOTICE 'last %', n; FETCH PRIOR FROM c INTO n; RAISE NOTICE 'prior %', n;
  RAISE EXCEPTION 'boom';
EXCEPTION WHEN raise_exception THEN
  FETCH FIRST FROM c INTO n; RAISE NOTICE 'handler %', n;
END; END $$;
CREATE FUNCTION f744(p integer) RETURNS SETOF text LANGUAGE plpgsql AS $$
BEGIN
  DECLARE c CURSOR (lim integer) FOR SELECT name FROM t744 WHERE id <= lim ORDER BY id; r record; BEGIN
    FOR r IN c(p) LOOP RETURN NEXT r.name; END LOOP;
  END;
END $$;
SELECT * FROM f744(2);
DO $$ BEGIN DECLARE c CURSOR FOR SELECT 1; x integer; BEGIN OPEN x; END; END $$;
DO $$ BEGIN DECLARE c CURSOR FOR SELECT 1; BEGIN c := 'twice744'; OPEN c; OPEN c; END; END $$;
DO $$ BEGIN DECLARE r record; BEGIN OPEN r; END; END $$;
DO $$ <<outer>> DECLARE c CURSOR FOR SELECT 'top' AS w; r record; BEGIN
  DECLARE c CURSOR FOR SELECT 'nested' AS w; BEGIN OPEN outer.c; FETCH outer.c INTO r; RAISE NOTICE '%', r; END;
END $$;
DO $$ DECLARE c CURSOR FOR SELECT id FROM t744 WHERE id = 2 OR name = 'c' ORDER BY id; r record; BEGIN FOR r IN c LOOP RAISE NOTICE '%', r; END LOOP; END $$;
