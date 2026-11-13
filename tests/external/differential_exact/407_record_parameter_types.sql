-- A PL/pgSQL `record` parameter takes the named composite type of its
-- argument: a NULL of that type reads its fields as NULL.
CREATE SCHEMA record_parameter_types;
SET search_path = record_parameter_types, public;
CREATE TABLE t (a int, b text);
CREATE FUNCTION f(r record) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  RETURN coalesce(r.a::text, 'null-a') || ',' || coalesce(r.b, 'null-b') || ',' || (r IS NULL)::text;
END $$;
SELECT f(NULL::t);
SELECT f(ROW(1, 'x')::t);
SELECT f(t) FROM (VALUES (2, 'y')) AS t(a, b);
CREATE FUNCTION g(r record) RETURNS text LANGUAGE plpgsql AS $$ BEGIN RETURN r.nosuch; END $$;
SELECT g(NULL::t);
CREATE FUNCTION h(r record) RETURNS record LANGUAGE plpgsql AS $$ BEGIN RETURN r; END $$;
SELECT h(NULL::t) IS NULL;
CREATE FUNCTION k(r anyelement) RETURNS text LANGUAGE plpgsql AS $$ BEGIN RETURN coalesce(r.b, 'nb'); END $$;
SELECT k(NULL::t);
DROP SCHEMA record_parameter_types CASCADE;
