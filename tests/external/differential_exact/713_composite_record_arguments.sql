-- An anonymous record (`ROW(...)`) is implicitly coercible to a composite
-- routine parameter (can_coerce_type), converted field by field; another
-- `record` expression is not (coerce_record_to_complex), and `record[]`
-- does not match a composite array parameter.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/parser/parse_coerce.c
CREATE TYPE ct AS (x integer, y text);
CREATE TABLE tt (a integer, b text);
CREATE FUNCTION k4(c ct) RETURNS text LANGUAGE sql AS $$ SELECT c.y || c.x $$;
CREATE FUNCTION k5(c tt) RETURNS text LANGUAGE plpgsql AS $$ BEGIN RETURN c.b || c.a; END $$;
CREATE FUNCTION k6(c ct[]) RETURNS integer LANGUAGE sql AS $$ SELECT cardinality(c) $$;
CREATE FUNCTION k7(c ct, d integer) RETURNS text LANGUAGE sql AS $$ SELECT c.y || d $$;
CREATE FUNCTION k7(c tt, d integer) RETURNS text LANGUAGE sql AS $$ SELECT c.b || d $$;
SELECT k4(ROW(1,'z'));
SELECT k4(ROW('2','w'));
SELECT k4(ROW(1,'z',3));
SELECT k4(ROW(1));
SELECT k4(ROW('a','z'));
SELECT k5(ROW(3, 'q'));
SELECT k5((4, 'r'));
SELECT k6(ARRAY[ROW(1,'a'), ROW(2,'b')]);
SELECT k7(ROW(1,'z'), 2);
SELECT k4(NULL::record);
SELECT k4(r) FROM (SELECT 1 AS x, 'n' AS y) r;
CREATE PROCEDURE p4(c ct) LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE '%', c; END $$;
CALL p4(ROW(5, 'v'));
DO $$ BEGIN RAISE NOTICE '%', k4(ROW(6, 'u')); END $$;
