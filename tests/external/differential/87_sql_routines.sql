DROP FUNCTION IF EXISTS routine_increment(integer);
DROP FUNCTION IF EXISTS routine_answer();

CREATE FUNCTION routine_answer() RETURNS integer LANGUAGE SQL AS 'SELECT 42';
CREATE FUNCTION routine_increment(value integer) RETURNS integer LANGUAGE SQL AS 'SELECT $1 + 1';

SELECT routine_answer(), routine_increment(41);
SELECT proname, pronargs, prorettype, prokind, proargtypes
  FROM pg_proc
 WHERE proname IN ('routine_answer', 'routine_increment')
 ORDER BY proname;
SELECT pg_function_is_visible(oid), pg_get_functiondef(oid) IS NOT NULL
  FROM pg_proc
 WHERE proname = 'routine_answer';
SELECT routine_name, routine_type, data_type, external_language
  FROM information_schema.routines
 WHERE routine_name IN ('routine_answer', 'routine_increment')
 ORDER BY routine_name;
SELECT parameter_name, parameter_mode, data_type
  FROM information_schema.parameters
 WHERE specific_name LIKE 'routine_increment_%';

BEGIN;
CREATE OR REPLACE FUNCTION routine_answer() RETURNS integer LANGUAGE SQL AS 'SELECT 43';
SELECT routine_answer();
ROLLBACK;
SELECT routine_answer();

DROP FUNCTION routine_increment(integer);
DROP FUNCTION routine_answer();
