DROP TABLE IF EXISTS procedure_log;
DROP PROCEDURE IF EXISTS procedure_log_value(integer);

CREATE TABLE procedure_log (value integer);
CREATE PROCEDURE procedure_log_value(value integer)
  LANGUAGE SQL AS 'INSERT INTO procedure_log VALUES ($1)';

CALL procedure_log_value(41);
SELECT value FROM procedure_log;
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
DROP TABLE procedure_log;
