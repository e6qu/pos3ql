-- PL/pgSQL analyses a statement's query as SPI prepares it: an analysis
-- error is positioned in the query and names it as its QUERY. A static OPEN
-- checks its cursor variable first. A field of a record variable's `record`
-- field names no column, even through an assignment.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/pl/plpgsql/src/pl_exec.c
CREATE TABLE tt (a integer);
CREATE FUNCTION open_field() RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'open_field'; r record;
BEGIN
  SELECT ROW(1, 2) AS payload INTO r;
  OPEN c FOR SELECT ((r.payload).f2)::integer;
  RETURN c;
END $$;
BEGIN;
SELECT open_field();
ROLLBACK;
CREATE FUNCTION open_plain_field() RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'open_plain_field'; r record;
BEGIN
  SELECT ROW(1, 2) AS payload INTO r;
  OPEN c FOR SELECT (r.payload).f2;
  RETURN c;
END $$;
BEGIN;
SELECT open_plain_field();
ROLLBACK;
CREATE FUNCTION open_missing() RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'open_missing';
BEGIN
  OPEN c FOR SELECT nosuch;
  RETURN c;
END $$;
BEGIN;
SELECT open_missing();
ROLLBACK;
CREATE FUNCTION open_operator() RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'open_operator'; r record;
BEGIN
  SELECT ROW(1, 2) AS payload INTO r;
  OPEN c FOR SELECT 1 + r.payload;
  RETURN c;
END $$;
BEGIN;
SELECT open_operator();
ROLLBACK;
CREATE FUNCTION open_in_use() RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'open_in_use';
BEGIN
  OPEN c FOR SELECT 1;
  OPEN c FOR SELECT nosuch;
  RETURN c;
END $$;
BEGIN;
SELECT open_in_use();
ROLLBACK;
CREATE FUNCTION assigned_field() RETURNS integer LANGUAGE plpgsql AS $$
DECLARE r record; v integer;
BEGIN
  SELECT ROW(1, 2) AS payload INTO r;
  v := (r.payload).f2;
  RETURN v;
END $$;
SELECT assigned_field();
CREATE FUNCTION inserted_field() RETURNS void LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
  SELECT ROW(1, 2) AS payload INTO r;
  INSERT INTO tt SELECT (r.payload).f2;
END $$;
SELECT inserted_field();
CREATE FUNCTION performed_field() RETURNS void LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
  SELECT ROW(1, 2) AS payload INTO r;
  PERFORM (r.payload).f2;
END $$;
SELECT performed_field();
DO $$DECLARE r record; BEGIN SELECT ROW(1, 2) AS payload INTO r; RAISE NOTICE '%', (r.payload).f1; END$$;
CREATE TYPE pair AS (x integer, y text);
CREATE TABLE ct (id integer, p pair);
INSERT INTO ct VALUES (1, ROW(5, 'a'));
CREATE FUNCTION named_fields() RETURNS text LANGUAGE plpgsql AS $$
DECLARE r record; q pair; v text; w ct;
BEGIN
  SELECT * INTO r FROM ct;
  v := (r.p).y;
  q := ROW(2, 'b');
  v := v || q.y || (q).x;
  SELECT * INTO w FROM ct;
  v := v || (w.p).x || ((w).p).y;
  SELECT ROW(1, 2) AS payload, 3 AS z INTO r;
  RETURN v || r.z;
END $$;
SELECT named_fields();
DROP FUNCTION open_field(), open_plain_field(), open_missing(), open_operator(),
  open_in_use(), assigned_field(), inserted_field(), performed_field(), named_fields();
DROP TABLE tt, ct;
DROP TYPE pair;
