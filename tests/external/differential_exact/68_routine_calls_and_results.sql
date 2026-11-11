-- PL/pgSQL CALL assigns output parameters to argument variables and follows
-- the calling chain's transaction-control rules; SQL function bodies are
-- type-checked at CREATE and, when deferred, at the call; polymorphic
-- built-ins resolve untyped literals before running; pg_sleep waits without
-- holding the SQL worker and returns once.
CREATE SCHEMA routine_calls;
SET search_path = routine_calls, public;
CREATE TABLE results(label text, a integer, b text);
CREATE PROCEDURE doubled(INOUT a int, OUT b text) LANGUAGE plpgsql
  AS $$BEGIN a := a * 2; b := 'b' || a; END$$;
CREATE PROCEDURE tripled(INOUT a int, OUT b text) LANGUAGE sql
  AS 'SELECT a * 3, ''s''';
DO $$DECLARE x int := 5; y text; BEGIN
  CALL doubled(x, y); INSERT INTO results VALUES ('static', x, y);
  x := 5; CALL tripled(x, y); INSERT INTO results VALUES ('sql', x, y);
  x := 5; y := NULL; EXECUTE 'CALL doubled($1, $2)' USING x, y;
  INSERT INTO results VALUES ('using', x, y);
  EXECUTE 'CALL doubled($1, $2)' INTO x, y USING x, y;
  INSERT INTO results VALUES ('into', x, y);
END$$;
SELECT * FROM results ORDER BY label;
DO $$DECLARE y text; BEGIN CALL doubled(5, y); END$$;
DO $$DECLARE x int := 1; BEGIN CALL doubled(a => x, b => NULL); END$$;
DO $$BEGIN CALL missing_procedure(1, 'a'); END$$;
CALL lower('a');
CREATE PROCEDURE commits() LANGUAGE plpgsql AS $$BEGIN
  INSERT INTO results VALUES ('commit', 10); COMMIT;
  INSERT INTO results VALUES ('rollback', 11); ROLLBACK; END$$;
CREATE FUNCTION calls_commit() RETURNS int LANGUAGE plpgsql
  AS $$BEGIN CALL commits(); RETURN 1; END$$;
DO $$BEGIN CALL commits(); END$$;
SELECT calls_commit();
BEGIN;
DO $$BEGIN CALL commits(); END$$;
ROLLBACK;
SELECT label, a FROM results WHERE label IN ('commit', 'rollback') ORDER BY label;
CREATE FUNCTION wide() RETURNS integer LANGUAGE sql AS 'SELECT 1, 2';
CREATE FUNCTION texty() RETURNS integer LANGUAGE sql AS 'SELECT ''a''::text';
CREATE FUNCTION untyped() RETURNS integer LANGUAGE sql AS 'SELECT ''1''';
CREATE FUNCTION narrows() RETURNS integer LANGUAGE sql AS 'SELECT 1::bigint';
CREATE FUNCTION outs(OUT a int, OUT b text) LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION poly(x anyelement) RETURNS anyelement LANGUAGE sql AS 'SELECT 1';
SELECT poly(true);
SELECT poly('a'::text);
SET check_function_bodies = off;
CREATE FUNCTION late() RETURNS integer LANGUAGE sql AS 'SELECT ''a''::text';
RESET check_function_bodies;
SELECT late();
SELECT array_replace('{a}'::varchar[], 'a', 'b'), array_replace(ARRAY[1], 1, 2.5);
SELECT array_append(ARRAY['2020-01-01'::date], '2020-01-02'), greatest(1, '2');
SELECT array_replace('{a}'::refcursor[], 'a', 'b');
SELECT array_append('{a}'::text[], 1);
SELECT DISTINCT x FROM (VALUES ('(1,2)'::point)) v(x);
SELECT '2020-01-01 00:00+00'::timestamptz + '200ms', '2020-01-01'::date - '2019-12-31';
SELECT '2020-01-01'::date + '1';
CREATE SEQUENCE sleep_numbers;
SELECT nextval('sleep_numbers'), pg_sleep(0.05), nextval('sleep_numbers'),
  pg_sleep_for('10 milliseconds'), pg_sleep(NULL) IS NULL;
RESET search_path;
DROP SCHEMA routine_calls CASCADE;
