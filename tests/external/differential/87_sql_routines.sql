DROP FUNCTION IF EXISTS routine_increment(integer);
DROP FUNCTION IF EXISTS routine_answer();
DROP FUNCTION IF EXISTS routine_lookup_value(integer);
DROP FUNCTION IF EXISTS routine_nested_value(integer);
DROP FUNCTION IF EXISTS routine_values_from(integer);
DROP FUNCTION IF EXISTS routine_pairs_from(integer);
DROP TABLE IF EXISTS routine_values;

CREATE FUNCTION routine_answer() RETURNS integer LANGUAGE SQL AS 'SELECT 42';
CREATE FUNCTION routine_increment(value integer) RETURNS integer LANGUAGE SQL AS 'SELECT $1 + 1';

SELECT routine_answer(), routine_increment(41);
CREATE TABLE routine_values (id integer PRIMARY KEY, value integer);
INSERT INTO routine_values VALUES (1, 40), (2, 41);
CREATE FUNCTION routine_lookup_value(integer) RETURNS integer LANGUAGE SQL
  AS 'SELECT value FROM routine_values WHERE id = $1';
CREATE FUNCTION routine_nested_value(integer) RETURNS integer LANGUAGE SQL
  AS 'SELECT routine_lookup_value($1) + 1';
CREATE FUNCTION routine_values_from(integer) RETURNS SETOF integer LANGUAGE SQL
  AS 'SELECT value FROM routine_values WHERE id >= $1';
CREATE FUNCTION routine_pairs_from(integer) RETURNS TABLE (routine_id integer, routine_value integer) LANGUAGE SQL
  AS 'SELECT id, value FROM routine_values WHERE id >= $1';
SELECT routine_lookup_value(1), routine_nested_value(2);
SELECT value FROM routine_values_from(1) AS values_from(value) ORDER BY value;
SELECT values_from.value, values_from.ordinality
  FROM routine_values_from(1) WITH ORDINALITY AS values_from(value, ordinality)
 ORDER BY values_from.ordinality;
SELECT routine_values.id, values_from.value
  FROM routine_values
  JOIN LATERAL routine_values_from(routine_values.id) AS values_from(value) ON true
 ORDER BY routine_values.id, values_from.value;
SELECT proretset FROM pg_proc WHERE proname = 'routine_values_from';
SELECT routine_id, routine_value FROM routine_pairs_from(1) ORDER BY routine_id;
SELECT routine_values.id, pairs.routine_value
  FROM routine_values
  JOIN LATERAL routine_pairs_from(routine_values.id) AS pairs ON true
 ORDER BY routine_values.id, pairs.routine_value;
SELECT routine_id, routine_value, ordinality
  FROM routine_pairs_from(1) WITH ORDINALITY
 ORDER BY ordinality;
SELECT proretset, prorettype FROM pg_proc WHERE proname = 'routine_pairs_from';
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
COMMIT;
SELECT routine_answer();

DROP FUNCTION routine_increment(integer);
DROP FUNCTION routine_answer();
DROP FUNCTION routine_lookup_value(integer);
DROP FUNCTION routine_nested_value(integer);
DROP FUNCTION routine_values_from(integer);
DROP FUNCTION routine_pairs_from(integer);
DROP TABLE routine_values;
