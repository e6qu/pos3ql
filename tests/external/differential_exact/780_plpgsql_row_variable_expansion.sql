-- PL/pgSQL row and record variables are parameters of every query level a
-- statement contains, so `(r).*` and `r.*` expand them inside derived
-- tables, WITH queries, set operations and nested subqueries. A row-typed
-- variable, or a record variable holding a named composite value, expands
-- its type's fields; a record variable holding an anonymous record, or
-- none, reaches the query as a parameter of type `record`, which parse
-- analysis cannot expand (42809). `name.*` that is both a FROM item and a
-- variable follows `plpgsql.variable_conflict`.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/pl/plpgsql/src/pl_comp.c
CREATE TYPE ct780 AS (a integer, b text);
CREATE TABLE t780 (a integer, b text);
INSERT INTO t780 VALUES (5, 'q');
DO $$ DECLARE r ct780; BEGIN r := ROW(1, 'x'); RAISE NOTICE '%', (SELECT s.a FROM (SELECT (r).*) s); END $$;
DO $$ DECLARE r record; BEGIN r := ROW(1, 'x')::ct780; RAISE NOTICE '%', (SELECT s.a FROM (SELECT (r).*) s); END $$;
DO $$ DECLARE r record; BEGIN r := ROW(1, 'x'); RAISE NOTICE '%', (SELECT s.f1 FROM (SELECT (r).*) s); END $$;
DO $$ DECLARE r record; v integer; BEGIN SELECT 1 AS a INTO r; SELECT s.a INTO v FROM (SELECT (r).*) s; END $$;
DO $$ DECLARE r ct780; v text; BEGIN r := ROW(1, 'x'); SELECT s.b INTO v FROM (SELECT (r).* FROM (SELECT 1) q) s; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r ct780; v text; BEGIN r := ROW(1, 'x'); SELECT s::text INTO v FROM (SELECT (r).*) s; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r ct780; v text; BEGIN r := ROW(1, 'x'); SELECT string_agg(b, ',') INTO v FROM (SELECT (r).* UNION ALL SELECT r.*) s; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r ct780; v integer; BEGIN r := ROW(1, 'x'); WITH w AS (SELECT (r).*) SELECT a INTO v FROM w; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r ct780; v integer; BEGIN r := ROW(2, 'x'); SELECT (SELECT (SELECT s.a FROM (SELECT (r).*) s)) INTO v; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r ct780; v integer; BEGIN SELECT s.c INTO v FROM (SELECT (r).*) s; END $$;
DO $$ DECLARE r ct780; v integer; BEGIN SELECT s.a INTO v FROM (SELECT (r).*) s; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r ct780; v text; BEGIN r := ROW(1, 'x'); SELECT s.b INTO v FROM (SELECT (r).*, count(*) FROM t780 GROUP BY 1, 2) s; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r ct780; v text; BEGIN r := ROW(1, 'x'); SELECT b INTO v FROM (SELECT DISTINCT (r).* FROM t780) s; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r ct780; v integer; BEGIN r := ROW(3, 'x'); SELECT max(a) INTO v FROM (SELECT (r).* FROM t780 ORDER BY 1, 2 LIMIT 1) s; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r ct780; v text; BEGIN r := ROW(1, 'x'); SELECT s.b INTO v FROM t780, LATERAL (SELECT (r).*, t780.a AS z) s; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r ct780; v text; BEGIN r := ROW(1, 'x'); FOR v IN SELECT s.b FROM (SELECT (r).*) s LOOP RAISE NOTICE '%', v; END LOOP; END $$;
DO $$ DECLARE r record; v integer; BEGIN FOR r IN SELECT * FROM t780 LOOP SELECT s.a INTO v FROM (SELECT (r).*) s; END LOOP; END $$;
-- `r.*` names the variable when no FROM item is called `r`.
DO $$ DECLARE r ct780; v integer; w text; BEGIN r := ROW(1, 'x'); SELECT r.* INTO v, w; RAISE NOTICE '% %', v, w; END $$;
DO $$ DECLARE r ct780; v text; BEGIN r := ROW(1, 'x'); SELECT s.b INTO v FROM (SELECT r.* FROM t780 q) s; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r record; v text; BEGIN r := ROW(1, 'x'); SELECT s.b INTO v FROM (SELECT r.*) s; END $$;
DO $$ DECLARE r ct780; v text; BEGIN r := ROW(1, 'x'); SELECT ROW(r.*)::text INTO v; RAISE NOTICE '%', v; END $$;
DO $$ DECLARE r ct780; v text; BEGIN r := ROW(1, 'x'); SELECT s.b INTO v FROM (SELECT r.*, t780.* FROM t780) s; END $$;
DO $$ DECLARE t780 ct780; v text; BEGIN t780 := ROW(1, 'x'); SELECT s.b INTO v FROM (SELECT t780.* FROM t780) s; END $$;
DO $$ #variable_conflict use_variable
DECLARE t780 ct780; v text; BEGIN t780 := ROW(1, 'x'); SELECT s.b INTO v FROM (SELECT t780.* FROM t780) s; RAISE NOTICE '%', v; END $$;
DO $$ #variable_conflict use_column
DECLARE t780 ct780; v text; BEGIN t780 := ROW(1, 'x'); SELECT s.b INTO v FROM (SELECT t780.* FROM t780) s; RAISE NOTICE '%', v; END $$;
CREATE FUNCTION f780(r ct780) RETURNS SETOF text LANGUAGE plpgsql AS $$ BEGIN RETURN QUERY SELECT s.b FROM (SELECT (r).*) s; END $$;
SELECT f780(ROW(2, 'w'));
CREATE FUNCTION g780() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE v text; w integer;
BEGIN
  SELECT x.a INTO w FROM (SELECT NEW.*) x; RAISE NOTICE 'new.* %', w;
  SELECT s.b INTO v FROM (SELECT (NEW).*) s; RAISE NOTICE '(new).* %', v;
  SELECT NEW.* INTO w, v; RAISE NOTICE 'into % %', w, v;
  RETURN NEW;
END $$;
CREATE TRIGGER g780 BEFORE INSERT ON t780 FOR EACH ROW EXECUTE FUNCTION g780();
INSERT INTO t780 VALUES (7, 'trig');
