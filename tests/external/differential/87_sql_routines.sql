DROP FUNCTION IF EXISTS routine_increment(integer);
DROP FUNCTION IF EXISTS routine_answer();
DROP FUNCTION IF EXISTS routine_lookup_value(integer);
DROP FUNCTION IF EXISTS routine_nested_value(integer);
DROP FUNCTION IF EXISTS routine_values_from(integer);
DROP FUNCTION IF EXISTS routine_pairs_from(integer);
DROP FUNCTION IF EXISTS routine_multi_query_value(integer);
DROP FUNCTION IF EXISTS routine_multi_query_pairs(integer);
DROP FUNCTION IF EXISTS routine_utility_prelude();
DROP FUNCTION IF EXISTS routine_update_all_values();
DROP FUNCTION IF EXISTS routine_set_void();
DROP FUNCTION IF EXISTS routine_correlated_write(integer);
DROP FUNCTION IF EXISTS routine_write_rows(integer);
DROP FUNCTION IF EXISTS routine_nested_write(integer);
DROP FUNCTION IF EXISTS routine_nested_write_result(integer);
DROP TABLE IF EXISTS routine_created_in_function;
DROP TABLE IF EXISTS routine_values;

CREATE TYPE routine_contract_state AS ENUM ('ready', 'done');
CREATE DOMAIN routine_contract_count AS integer CHECK (VALUE > 0);
CREATE TYPE routine_contract_pair AS (value integer, label text);
CREATE FUNCTION routine_contract_state_echo(value routine_contract_state) RETURNS routine_contract_state LANGUAGE SQL AS 'SELECT $1';
CREATE FUNCTION routine_contract_count_echo(value routine_contract_count) RETURNS routine_contract_count LANGUAGE SQL AS 'SELECT $1';
CREATE FUNCTION routine_contract_pair_echo(value routine_contract_pair) RETURNS routine_contract_pair LANGUAGE SQL AS 'SELECT $1';
CREATE FUNCTION routine_contract_state_array_echo(value routine_contract_state[]) RETURNS routine_contract_state[] LANGUAGE SQL AS 'SELECT $1';
CREATE FUNCTION routine_contract_count_array_echo(value routine_contract_count[]) RETURNS routine_contract_count[] LANGUAGE SQL AS 'SELECT $1';
CREATE FUNCTION routine_contract_pair_array_echo(value routine_contract_pair[]) RETURNS routine_contract_pair[] LANGUAGE SQL AS 'SELECT $1';
CREATE FUNCTION routine_contract_overload(value integer) RETURNS text LANGUAGE SQL AS 'SELECT ''integer''';
CREATE FUNCTION routine_contract_overload(value routine_contract_count) RETURNS text LANGUAGE SQL AS 'SELECT ''domain''';
SELECT routine_contract_state_echo('ready'::routine_contract_state)::text,
       routine_contract_count_echo(3::routine_contract_count)::text,
       routine_contract_pair_echo(ROW(4, 'four')::routine_contract_pair)::text,
       routine_contract_state_array_echo(ARRAY['done'::routine_contract_state])::text,
       routine_contract_count_array_echo(ARRAY[5::routine_contract_count])::text,
       routine_contract_pair_array_echo(ARRAY[ROW(6, 'six')::routine_contract_pair])::text,
       routine_contract_overload(1), routine_contract_overload(1::routine_contract_count);
SELECT pg_get_function_arguments('routine_contract_count_echo(routine_contract_count)'::regprocedure),
       pg_get_function_result('routine_contract_count_echo(routine_contract_count)'::regprocedure),
       'routine_contract_overload(integer)'::regprocedure::text,
       'routine_contract_overload(routine_contract_count)'::regprocedure::text;
DROP FUNCTION routine_contract_state_echo(routine_contract_state);
DROP FUNCTION routine_contract_count_echo(routine_contract_count);
DROP FUNCTION routine_contract_pair_echo(routine_contract_pair);
DROP FUNCTION routine_contract_state_array_echo(routine_contract_state []);
DROP FUNCTION routine_contract_count_array_echo(routine_contract_count []);
DROP FUNCTION routine_contract_pair_array_echo(routine_contract_pair []);
DROP FUNCTION routine_contract_overload(integer);
DROP FUNCTION routine_contract_overload(routine_contract_count);
DROP TYPE routine_contract_pair;
DROP DOMAIN routine_contract_count;
DROP TYPE routine_contract_state;

CREATE FUNCTION routine_answer() RETURNS integer LANGUAGE SQL AS 'SELECT 42';
CREATE FUNCTION routine_increment(value integer) RETURNS integer LANGUAGE SQL AS 'SELECT $1 + 1';

SELECT routine_answer(), routine_increment(41);
CREATE FUNCTION routine_multi_query_value(integer) RETURNS integer LANGUAGE SQL
  AS 'SELECT 1 / $1 UNION ALL SELECT 2 / $1; WITH routine_input AS (SELECT $1 AS value), derived_input AS (SELECT value + 1 AS value FROM routine_input) SELECT value + 1 FROM derived_input';
CREATE FUNCTION routine_multi_query_pairs(integer) RETURNS TABLE (routine_id integer, routine_value integer) LANGUAGE SQL
  AS 'SELECT $1; WITH first_pair AS (SELECT $1 AS value), derived_pair AS (SELECT value, value + 1 AS next_value FROM first_pair) SELECT value, next_value FROM derived_pair UNION ALL SELECT $1 + 1, $1 + 2';
SELECT routine_multi_query_value(40);
SELECT routine_id, routine_value FROM routine_multi_query_pairs(7) ORDER BY routine_id;
SELECT routine_multi_query_value(0);
DROP FUNCTION routine_multi_query_value(integer);
DROP FUNCTION routine_multi_query_pairs(integer);
CREATE TABLE routine_values (id integer PRIMARY KEY, value integer);
INSERT INTO routine_values VALUES (1, 40), (2, 41);
CREATE FUNCTION routine_correlated_write(integer) RETURNS integer LANGUAGE SQL
  AS 'UPDATE routine_values SET value = value + 1 WHERE id = $1 RETURNING value';
BEGIN;
SELECT routine_correlated_write(id) FROM routine_values ORDER BY id;
SELECT id, value FROM routine_values ORDER BY id;
ROLLBACK;
DROP FUNCTION routine_correlated_write(integer);
CREATE FUNCTION routine_write_rows(integer) RETURNS SETOF integer LANGUAGE SQL
  AS 'UPDATE routine_values SET value = value + 1 WHERE id = $1 RETURNING value';
BEGIN;
SELECT value FROM routine_write_rows(1) AS result(value);
SELECT id, value FROM routine_values WHERE id = 1;
ROLLBACK;
DROP FUNCTION routine_write_rows(integer);
CREATE FUNCTION routine_nested_write(integer) RETURNS integer LANGUAGE SQL
  AS 'UPDATE routine_values SET value = value + 1 WHERE id = $1 RETURNING value';
CREATE FUNCTION routine_nested_write_result(integer) RETURNS integer LANGUAGE SQL
  AS 'SELECT routine_nested_write($1)';
BEGIN;
SELECT routine_nested_write_result(1);
SELECT id, value FROM routine_values WHERE id = 1;
ROLLBACK;
DROP FUNCTION routine_nested_write_result(integer);
DROP FUNCTION routine_nested_write(integer);
CREATE FUNCTION routine_update_all_values() RETURNS integer LANGUAGE SQL
  AS 'UPDATE routine_values SET value = value + 1 RETURNING value';
SELECT routine_update_all_values();
SELECT id, value FROM routine_values ORDER BY id;
CREATE FUNCTION routine_lookup_value(integer) RETURNS integer LANGUAGE SQL
  AS 'SELECT value FROM routine_values WHERE id = $1';
CREATE FUNCTION routine_nested_value(integer) RETURNS integer LANGUAGE SQL
  AS 'SELECT routine_lookup_value($1) + 1';
CREATE FUNCTION routine_values_from(integer) RETURNS SETOF integer LANGUAGE SQL
  AS 'SELECT value FROM routine_values WHERE id >= $1';
CREATE FUNCTION routine_set_void() RETURNS SETOF void LANGUAGE SQL
  AS 'SELECT NULL UNION ALL SELECT NULL';
CREATE FUNCTION routine_pairs_from(integer) RETURNS TABLE (routine_id integer, routine_value integer) LANGUAGE SQL
  AS 'SELECT id, value FROM routine_values WHERE id >= $1';
SELECT routine_lookup_value(1), routine_nested_value(2);
CREATE OR REPLACE FUNCTION routine_multi_query_value(integer) RETURNS integer LANGUAGE SQL
  AS 'INSERT INTO routine_values VALUES (3, $1); WITH inserted_value AS (SELECT value FROM routine_values WHERE id = 3) SELECT value + 2 FROM inserted_value';
SELECT routine_multi_query_value(40);
SELECT id, value FROM routine_values WHERE id = 3;
CREATE FUNCTION routine_returning_value(integer) RETURNS integer LANGUAGE SQL
  AS 'INSERT INTO routine_values VALUES (5, $1) RETURNING value + 2';
SELECT routine_returning_value(40);
SELECT id, value FROM routine_values WHERE id = 5;
CREATE FUNCTION routine_utility_prelude() RETURNS integer LANGUAGE SQL
  AS 'CREATE TABLE routine_created_in_function (value integer); SELECT 44';
SELECT routine_utility_prelude();
INSERT INTO routine_created_in_function VALUES (45);
SELECT value FROM routine_created_in_function;
CREATE SCHEMA routine_path;
SET search_path TO routine_path, public;
CREATE FUNCTION write_on_path(integer) RETURNS integer LANGUAGE SQL
  AS 'INSERT INTO public.routine_values VALUES (4, $1); WITH inserted_value AS (SELECT value FROM public.routine_values WHERE id = 4) SELECT value + 2 FROM inserted_value';
SELECT write_on_path(60);
SET search_path TO public;
SELECT id, value FROM routine_values WHERE id = 4;
SELECT value FROM routine_values_from(1) AS values_from(value) ORDER BY value;
SELECT values_from.value, values_from.ordinality
  FROM routine_values_from(1) WITH ORDINALITY AS values_from(value, ordinality)
 ORDER BY values_from.ordinality;
SELECT routine_values.id, values_from.value
  FROM routine_values
  JOIN LATERAL routine_values_from(routine_values.id) AS values_from(value) ON true
 ORDER BY routine_values.id, values_from.value;
SELECT proretset FROM pg_proc WHERE proname = 'routine_values_from';
SELECT count(*) FROM routine_set_void() AS result(value);
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
DROP FUNCTION routine_utility_prelude();
DROP FUNCTION routine_update_all_values();
DROP FUNCTION routine_set_void();
DROP TABLE routine_created_in_function;
DROP TABLE routine_values;
