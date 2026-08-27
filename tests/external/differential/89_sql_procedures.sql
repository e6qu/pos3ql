DROP TABLE IF EXISTS procedure_log;
DROP PROCEDURE IF EXISTS procedure_log_value(integer);
DROP PROCEDURE IF EXISTS procedure_log_pair(integer);

CREATE TABLE procedure_log (value integer);
CREATE PROCEDURE procedure_log_value(value integer)
  LANGUAGE SQL AS 'INSERT INTO procedure_log VALUES ($1)';

CALL procedure_log_value(41);
SELECT value FROM procedure_log;
CREATE PROCEDURE procedure_log_pair(value integer)
  LANGUAGE SQL AS 'WITH routine_input AS (SELECT $1 AS value), incremented_input AS (SELECT value + 1 AS value FROM routine_input) INSERT INTO procedure_log SELECT value FROM routine_input UNION ALL SELECT value FROM incremented_input';
CALL procedure_log_pair(42);
CREATE PROCEDURE procedure_defaults(a integer, b integer DEFAULT 2)
  LANGUAGE SQL AS 'INSERT INTO procedure_log VALUES (a + b)';
CREATE PROCEDURE procedure_output(IN a integer, OUT doubled integer, INOUT total integer,
                                  IN b integer DEFAULT 2)
  LANGUAGE SQL AS 'SELECT a * 2, total + b';
CREATE PROCEDURE procedure_variadic(prefix integer, VARIADIC vals integer[])
  LANGUAGE SQL AS 'INSERT INTO procedure_log VALUES (prefix + cardinality(vals))';
CALL procedure_defaults(3);
CALL procedure_defaults(b => 5, a => 4);
CALL procedure_output(3, NULL, 10);
CALL procedure_variadic(10, 1, 2, 3);
CALL procedure_variadic(20, VARIADIC ARRAY[1, 2]);
SELECT value FROM procedure_log ORDER BY value;
SELECT proname, pronargs, prorettype, prokind, proargtypes
  FROM pg_proc
 WHERE proname = 'procedure_log_value';
SELECT pg_get_functiondef(oid) IS NOT NULL
  FROM pg_proc
 WHERE proname = 'procedure_log_value';
SELECT routine_name, routine_type, data_type, external_language
  FROM information_schema.routines
 WHERE routine_name = 'procedure_log_value';
SELECT parameter_name, parameter_mode, data_type
  FROM information_schema.parameters
 WHERE specific_name LIKE 'procedure_log_value_%';

DROP PROCEDURE procedure_log_value(integer);
DROP PROCEDURE procedure_log_pair(integer);
DROP PROCEDURE procedure_defaults(integer, integer);
DROP PROCEDURE procedure_output(integer, integer, integer);
DROP PROCEDURE procedure_variadic(integer, integer[]);
DROP TABLE procedure_log;
