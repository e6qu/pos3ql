-- A routine's formal parameters and PL locals retain its input collation.
CREATE SEQUENCE parameter_checks;
CREATE TYPE parameter_choice AS ENUM('a');
CREATE DOMAIN parameter_checked AS parameter_choice CHECK(nextval('parameter_checks')>0);
CREATE FUNCTION parameter_inner(input text) RETURNS parameter_checked[]
  LANGUAGE sql SET search_path=public AS $$SELECT '{a}'::parameter_checked[]$$;
CREATE FUNCTION parameter_sql(input text) RETURNS parameter_checked[]
  LANGUAGE sql SET search_path=public AS $$SELECT parameter_inner(input)$$;
CREATE FUNCTION parameter_pl(input text) RETURNS parameter_checked[]
  LANGUAGE plpgsql AS $$BEGIN RETURN parameter_inner(input); END$$;
CREATE FUNCTION parameter_pl_table(input text) RETURNS SETOF parameter_checked[]
  LANGUAGE plpgsql AS $$BEGIN RETURN NEXT parameter_inner(input); END$$;
CREATE FUNCTION parameter_local(input text) RETURNS parameter_checked[]
  LANGUAGE plpgsql AS $$DECLARE copied text := input; BEGIN RETURN parameter_inner(copied); END$$;
CREATE PROCEDURE parameter_sql_procedure(input text)
  LANGUAGE sql SET search_path=public AS $$SELECT parameter_inner(input)$$;
CREATE PROCEDURE parameter_pl_procedure(input text)
  LANGUAGE plpgsql AS $$BEGIN PERFORM parameter_inner(input); END$$;
ALTER SEQUENCE parameter_checks RESTART WITH 1;
SELECT parameter_sql('a' COLLATE "C");
SELECT 'sql_c',last_value FROM parameter_checks;
SELECT parameter_sql('a' COLLATE "POSIX");
SELECT 'sql_posix',last_value FROM parameter_checks;
SELECT parameter_sql('a' COLLATE "C");
SELECT 'sql_c_reused',last_value FROM parameter_checks;
DISCARD PLANS;
ALTER SEQUENCE parameter_checks RESTART WITH 1;
SELECT parameter_pl('a' COLLATE "C");
SELECT 'pl_c',last_value FROM parameter_checks;
SELECT parameter_pl('a' COLLATE "POSIX");
SELECT 'pl_posix',last_value FROM parameter_checks;
SELECT parameter_pl('a' COLLATE "C");
SELECT 'pl_c_reused',last_value FROM parameter_checks;
SELECT * FROM parameter_pl_table('a' COLLATE "C");
SELECT * FROM parameter_pl_table('a' COLLATE "POSIX");
SELECT 'table_function_retains',last_value FROM parameter_checks;
SELECT parameter_local('a' COLLATE "C");
SELECT parameter_local('a' COLLATE "POSIX");
SELECT 'local_retains',last_value FROM parameter_checks;
CALL parameter_sql_procedure('a' COLLATE "C");
CALL parameter_sql_procedure('a' COLLATE "POSIX");
SELECT 'sql_procedure_retains',last_value FROM parameter_checks;
CALL parameter_pl_procedure('a' COLLATE "C");
CALL parameter_pl_procedure('a' COLLATE "POSIX");
SELECT 'pl_procedure_retains',last_value FROM parameter_checks;
PREPARE parameter_statement(text) AS SELECT parameter_sql($1 COLLATE "C");
EXECUTE parameter_statement('a');
EXECUTE parameter_statement('b');
SELECT 'prepared_retains',last_value FROM parameter_checks;
DEALLOCATE parameter_statement;
DROP PROCEDURE parameter_sql_procedure(text),parameter_pl_procedure(text);
DROP FUNCTION parameter_sql(text),parameter_pl(text),parameter_pl_table(text),parameter_local(text);
DROP FUNCTION parameter_inner(text);
DROP DOMAIN parameter_checked;
DROP TYPE parameter_choice;
DROP SEQUENCE parameter_checks;
