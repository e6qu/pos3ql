-- Column definition lists for functions returning record: the list gives
-- the result columns the function's rows must match, and PostgreSQL's
-- errors for a missing, redundant or misplaced list.
CREATE SCHEMA column_definition_lists;
SET search_path = column_definition_lists, public;
CREATE FUNCTION sr() RETURNS SETOF record LANGUAGE plpgsql AS $$ BEGIN RETURN QUERY SELECT 1, 'a'::text; RETURN QUERY SELECT 2, 'b'::text; END $$;
SELECT * FROM sr() AS (a int, b text);
SELECT * FROM sr() AS t(a int, b text) WHERE a > 1;
SELECT t.b FROM sr() t(a int, b text);
SELECT * FROM sr();
SELECT * FROM sr() AS (a int, b int);
SELECT * FROM sr() AS (a int);
CREATE FUNCTION si() RETURNS SETOF int LANGUAGE sql AS $$ SELECT 1 $$;
SELECT * FROM si() AS (a int);
CREATE FUNCTION r1() RETURNS record LANGUAGE sql AS $$ SELECT 1, 'x'::text $$;
SELECT * FROM r1() AS (a int, b text);
SELECT * FROM r1() AS (a int, b varchar);
CREATE FUNCTION so(OUT a int, OUT b text) RETURNS SETOF record LANGUAGE sql AS $$ SELECT 1, 'z' $$;
SELECT * FROM so() AS (a int, b text);
SELECT * FROM sr() AS (a int, a text);
CREATE VIEW crv AS SELECT * FROM sr() AS t(a int, b text);
SELECT * FROM crv;
SELECT * FROM json_to_record('{"a":1,"b":"q"}') AS x(a int, b text);
SELECT * FROM jsonb_to_recordset('[{"a":1},{"a":2}]') AS x(a int);
SELECT * FROM ROWS FROM (sr() AS (a int, b text));
CREATE FUNCTION sq() RETURNS SETOF record LANGUAGE sql AS $$ VALUES (1, 'p'::text), (2, 'q') $$;
SELECT * FROM sq() AS s(n int, label text) ORDER BY n DESC;
CREATE FUNCTION rn() RETURNS SETOF record LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
  FOR r IN SELECT g, g * 10 FROM generate_series(1, 2) g LOOP RETURN NEXT r; END LOOP;
END $$;
SELECT * FROM rn() AS (g int, tens int);
CREATE FUNCTION tq() RETURNS TABLE (a int, b int) LANGUAGE plpgsql AS $$ BEGIN RETURN QUERY SELECT 1, 'x'::text; END $$;
SELECT * FROM tq();
SELECT * FROM sr() AS (a int, b text) JOIN sq() AS (n int, label text) ON a = n;
DROP SCHEMA column_definition_lists CASCADE;
