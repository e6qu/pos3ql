-- PL/pgSQL row variables: `VALUES (r.*)` expands the variable into its
-- fields; a missing field is reported by its record, without a position,
-- under a "PL/pgSQL expression" context, also when the variable holds no
-- value yet. A row compared with a subquery of another type reports the
-- operator at the IN or quantified operator with PostgreSQL's hint.
CREATE TABLE r190 (a int, b text);
CREATE TYPE c190 AS (x int, y text);
CREATE FUNCTION f190_values() RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE r r190; c c190;
BEGIN
  r := ROW(1, 'x');
  INSERT INTO r190 VALUES (r.*);
  c := ROW(2, 'y');
  INSERT INTO r190 VALUES (c.*), (3, 'z');
  RETURN (SELECT count(*) FROM r190);
END $$;
SELECT f190_values();
SELECT * FROM r190 ORDER BY a;
CREATE FUNCTION f190_unassigned_row() RETURNS text LANGUAGE plpgsql AS $$
DECLARE r r190;
BEGIN
  IF r.a IS NULL THEN RAISE NOTICE 'a is null'; END IF;
  RETURN r.nosuch;
END $$;
SELECT f190_unassigned_row();
CREATE FUNCTION f190_unassigned_composite() RETURNS text LANGUAGE plpgsql AS $$
DECLARE c c190;
BEGIN
  RETURN c.nosuch;
END $$;
SELECT f190_unassigned_composite();
CREATE FUNCTION f190_assigned() RETURNS text LANGUAGE plpgsql AS $$
DECLARE r r190;
BEGIN
  r := ROW(1, 'x');
  RETURN r.nosuch;
END $$;
SELECT f190_assigned();
CREATE FUNCTION f190_subquery() RETURNS text LANGUAGE plpgsql AS $$
DECLARE r r190;
BEGIN
  r := ROW(1, 'x');
  RETURN (SELECT r.nosuch);
END $$;
SELECT f190_subquery();
CREATE FUNCTION f190_record() RETURNS text LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
  RETURN r.a;
END $$;
SELECT f190_record();
SELECT ROW(1.0) IN (SELECT ROW(1.00));
SELECT ROW(1.0) = ANY (SELECT ROW(1.00));
SELECT 1 IN (SELECT 'a'::text WHERE false);
DROP ROUTINE f190_values(), f190_unassigned_row(), f190_unassigned_composite(), f190_assigned(), f190_subquery(), f190_record() CASCADE;
