-- `(r).*` of a PL/pgSQL record variable: an expression is the target list
-- of the query PostgreSQL makes of it, so the record's fields are its
-- columns, one of them its value and another count an error. A `record`
-- variable holding an anonymous record, or none, reaches the query as a
-- parameter of type `record`, whose fields parse analysis cannot expand
-- (42809). A `.*` outside a target list or `ROW(...)` fails analysis at its
-- position.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/fmgr/funcapi.c
CREATE TYPE ct743 AS (x integer, y text);
CREATE TYPE c1743 AS (x integer);
DO $$ DECLARE r record; BEGIN r := ROW(1, 2); RAISE NOTICE '%', (r).*; END $$;
DO $$ DECLARE r record; v integer; BEGIN r := ROW(1, 2); SELECT (r).* INTO v; END $$;
DO $$ DECLARE r record; BEGIN SELECT 1 AS x, 2 AS y INTO r; RAISE NOTICE '%', (r).*; END $$;
DO $$ DECLARE r record; v record; BEGIN SELECT 1 AS x, 2 AS y INTO r; SELECT (r).* INTO v; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r record; BEGIN r := ROW(1, 2); PERFORM (r).*; END $$;
DO $$ DECLARE r record; BEGIN RAISE NOTICE '%', (r).*; END $$;
DO $$ DECLARE r record; v record; BEGIN r := ROW(1, 'a')::ct743; SELECT (r).* INTO v; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r ct743; v record; BEGIN r := ROW(1, 'a'); SELECT (r).* INTO v; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r record; BEGIN r := ROW(1, 'a')::ct743; RAISE NOTICE '%', (r).*; END $$;
DO $$ DECLARE r record; BEGIN r := ROW(1)::c1743; RAISE NOTICE '%', (r).*; END $$;
DO $$ DECLARE r record; v integer; BEGIN r := ROW(1)::c1743; v := (r).*; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r record; a integer; b text; BEGIN r := ROW(1, 'a')::ct743; SELECT (r).* INTO a, b; RAISE NOTICE '% %', a, b; END $$;
DO $$ DECLARE r record; BEGIN r := ROW(1, 'a')::ct743; IF (r).* IS NOT NULL THEN RAISE NOTICE 'x'; END IF; END $$;
DO $$ DECLARE r record; BEGIN r := ROW(1, 'a'); IF (r).* IS NOT NULL THEN RAISE NOTICE 'x'; END IF; END $$;
DO $$ DECLARE r record; BEGIN r := ROW(1, 'a'); PERFORM 1 WHERE (r).* IS NOT NULL; END $$;
DO $$ DECLARE r ct743; BEGIN r := ROW(1, 'a'); RAISE NOTICE '%', (r).*; END $$;
