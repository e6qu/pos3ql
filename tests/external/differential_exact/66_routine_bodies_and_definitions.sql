-- SQL-language bodies must end with a statement that produces the declared
-- result. PostgreSQL names the type as format_type_be does, calls procedure
-- outputs a record, defers the check to the call when check_function_bodies is
-- off, and rejects transaction control only when a call reaches it.
CREATE SCHEMA routine_defs;
CREATE TYPE routine_defs."Pair Type" AS (a integer);
CREATE TABLE routine_defs.result_rows(a integer);
CREATE FUNCTION routine_defs.r() RETURNS integer LANGUAGE sql AS '';
CREATE FUNCTION routine_defs.r() RETURNS integer LANGUAGE sql BEGIN ATOMIC END;
CREATE FUNCTION routine_defs.r() RETURNS integer LANGUAGE sql
  AS 'SELECT 1; INSERT INTO routine_defs.result_rows VALUES (1)';
CREATE FUNCTION routine_defs.r() RETURNS SETOF timestamptz LANGUAGE sql AS ';';
CREATE FUNCTION routine_defs.r(OUT a integer, OUT b integer) LANGUAGE sql AS '';
CREATE FUNCTION routine_defs.r() RETURNS TABLE(a integer) LANGUAGE sql AS '';
CREATE FUNCTION routine_defs.r() RETURNS routine_defs."Pair Type" LANGUAGE sql AS '';
CREATE FUNCTION routine_defs.r() RETURNS varchar(4)[] LANGUAGE sql AS 'SELECT 1; COMMIT';
CREATE PROCEDURE routine_defs.r(OUT a integer) LANGUAGE sql AS '';
CREATE PROCEDURE routine_defs.r(INOUT a integer) LANGUAGE sql BEGIN ATOMIC END;
SET check_function_bodies = off;
CREATE FUNCTION routine_defs.deferred() RETURNS integer LANGUAGE sql BEGIN ATOMIC END;
CREATE PROCEDURE routine_defs.deferred_output(OUT a integer) LANGUAGE sql AS '';
CREATE FUNCTION routine_defs.deferred_wide() RETURNS integer LANGUAGE sql AS 'SELECT 1, 2';
RESET check_function_bodies;
SELECT routine_defs.deferred();
CALL routine_defs.deferred_output(NULL);
SELECT routine_defs.deferred_wide();
CREATE PROCEDURE routine_defs.first_row(INOUT a integer) LANGUAGE sql
  AS 'SELECT * FROM generate_series(a, a + 2)';
CALL routine_defs.first_row(4);
CREATE PROCEDURE routine_defs.returned_row(INOUT a integer, OUT b text) LANGUAGE sql
  AS 'INSERT INTO routine_defs.result_rows VALUES (a), (a + 1) RETURNING a, ''x''';
CALL routine_defs.returned_row(7, NULL);
CREATE PROCEDURE routine_defs.savepoint() LANGUAGE sql
  AS 'INSERT INTO routine_defs.result_rows VALUES (1); SAVEPOINT a';
CALL routine_defs.savepoint();
CREATE PROCEDURE routine_defs.started() LANGUAGE sql AS 'START TRANSACTION';
CALL routine_defs.started();
CREATE FUNCTION routine_defs.vacuum() RETURNS void LANGUAGE sql AS 'VACUUM';
SELECT routine_defs.vacuum();
SELECT a FROM routine_defs.result_rows ORDER BY a;
START TRANSACTION;
COMMIT;
DROP SCHEMA routine_defs CASCADE;

-- pg_get_functiondef prints PostgreSQL's multi-line header, only non-default
-- attributes, one SET line per setting with list elements quoted separately,
-- and a dollar-quoted string body.
CREATE SCHEMA function_defs;
CREATE TYPE function_defs."My Type" AS (a integer);
CREATE FUNCTION function_defs.d1(a int, b int DEFAULT 1 + 2, OUT c int)
  LANGUAGE sql IMMUTABLE STRICT AS 'SELECT a';
CREATE FUNCTION function_defs.d2() RETURNS SETOF int LANGUAGE sql STABLE LEAKPROOF
  SECURITY DEFINER PARALLEL SAFE COST 5 ROWS 7
  SET search_path = public, "My Schema", pg_temp SET datestyle = 'iso, mdy' AS 'SELECT 1';
CREATE FUNCTION function_defs.d3() RETURNS int LANGUAGE sql RETURN 1;
CREATE PROCEDURE function_defs.d4(INOUT a int, VARIADIC b int[]) LANGUAGE sql AS $$SELECT a$$;
CREATE PROCEDURE function_defs.d5() LANGUAGE sql BEGIN ATOMIC END;
CREATE FUNCTION function_defs.d6() RETURNS void LANGUAGE plpgsql PARALLEL RESTRICTED
  COST 1000 AS $$begin end$$;
CREATE FUNCTION function_defs.d7() RETURNS int LANGUAGE sql
  AS $q$SELECT 1 /* $function$ $functionx$ */$q$;
CREATE FUNCTION function_defs.d8() RETURNS SETOF int LANGUAGE sql ROWS 1000 COST 100
  AS 'SELECT 1';
CREATE FUNCTION function_defs.d9(a int, b int, OUT c int, OUT d int) RETURNS SETOF record
  LANGUAGE sql AS 'SELECT 1, 2';
CREATE FUNCTION function_defs.d10() RETURNS int LANGUAGE sql STRICT PARALLEL SAFE
  AS 'SELECT 1';
CREATE FUNCTION function_defs.d11() RETURNS int LANGUAGE sql COST 1000000 AS 'SELECT 1';
CREATE FUNCTION function_defs.d12() RETURNS int LANGUAGE sql COST 0.00001 AS 'SELECT 1';
CREATE FUNCTION function_defs.d13() RETURNS setof int LANGUAGE sql COST 123.456789 ROWS 2.5
  AS 'SELECT 1';
CREATE FUNCTION function_defs."T f"("A b" function_defs."My Type")
  RETURNS TABLE("X y" function_defs."My Type", z int) LANGUAGE sql
  AS 'SELECT null::function_defs."My Type", 1';
CREATE PROCEDURE function_defs."Pr oc"("A b" int, INOUT "c" function_defs."My Type"[])
  LANGUAGE sql AS 'SELECT c';
CREATE FUNCTION function_defs.d14() RETURNS int LANGUAGE sql
  SET search_path = '' SET timezone = 'UTC' AS 'SELECT 1';
SELECT pg_get_functiondef(p.oid)
  FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
 WHERE nspname = 'function_defs' ORDER BY proname;
SELECT proname, pg_get_function_arguments(p.oid), pg_get_function_identity_arguments(p.oid),
       pg_get_function_result(p.oid)
  FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
 WHERE nspname = 'function_defs' ORDER BY proname;
DROP SCHEMA function_defs CASCADE;
