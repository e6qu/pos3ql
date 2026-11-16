-- PL/pgSQL passes a `record` variable holding an anonymous record to a query
-- as a `record` parameter, whose fields parse analysis cannot name:
-- `(p).f1` fails when the expression or statement is planned, while `p.f1`
-- and a variable holding a named composite resolve. A bound cursor's query
-- is analysed in its declaration's namespace and sees only the variables
-- declared before the cursor.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/pl/plpgsql/src/pl_comp.c
DO $$ DECLARE p record; BEGIN p := ROW(1, 'a'); RAISE NOTICE '%', (p).f1; END $$;
DO $$ DECLARE p record; BEGIN p := ROW(1, 'a'); RAISE NOTICE '%', p.f1; END $$;
DO $$ DECLARE p record; BEGIN p := ROW(1, 'a'); RAISE NOTICE 'start'; RAISE NOTICE '%', (p).f2; END $$;
DO $$ DECLARE p record; v integer; BEGIN p := ROW(1, 'a'); v := (p).f1; END $$;
DO $$ DECLARE p record; v integer; BEGIN p := ROW(1, 'a'); SELECT (p).f1 INTO v; END $$;
DO $$ DECLARE p record; BEGIN SELECT 1 AS f1, 'a' AS f2 INTO p; RAISE NOTICE '%', (p).f1; END $$;
DO $$ DECLARE p record; BEGIN p := (SELECT ROW(1, 'b')); RAISE NOTICE '%', p; END $$;
CREATE TYPE ct AS (x integer, y text);
DO $$ DECLARE p record; BEGIN p := ROW(1, 'a')::ct; RAISE NOTICE '%', (p).x; END $$;
DO $$ DECLARE p ct; BEGIN p := ROW(1, 'a'); RAISE NOTICE '%', (p).x; END $$;
DO $$ DECLARE p record; BEGIN p := ROW(1, 'a'); IF (p).f1 = 1 THEN RAISE NOTICE 'one'; END IF; END $$;
CREATE FUNCTION fr() RETURNS integer LANGUAGE plpgsql AS $$ DECLARE p record; BEGIN p := ROW(7, 'z'); RETURN (p).f1; END $$;
SELECT fr();
DO $$ DECLARE c CURSOR FOR SELECT v1; v1 integer := 5; BEGIN OPEN c; END $$;
DO $$ DECLARE v0 integer := 3; c CURSOR FOR SELECT v0 + v1; v1 integer := 5; r record; BEGIN OPEN c; FETCH c INTO r; RAISE NOTICE '%', r; END $$;
DO $$ DECLARE c CURSOR (k integer) FOR SELECT k + v1; v1 integer := 5; BEGIN OPEN c(1); END $$;
DO $$ DECLARE c CURSOR FOR SELECT v1; v1 integer := 5; r record; BEGIN FOR r IN c LOOP RAISE NOTICE '%', r; END LOOP; END $$;
DO $$ DECLARE c CURSOR FOR SELECT 1; v1 integer := 5; r record; BEGIN OPEN c; FETCH c INTO r; RAISE NOTICE '%', r; END $$;
DO $$ DECLARE c refcursor; v1 integer := 5; r record; BEGIN OPEN c FOR SELECT v1; FETCH c INTO r; RAISE NOTICE '%', r; END $$;
