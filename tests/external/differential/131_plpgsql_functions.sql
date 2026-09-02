-- Scalar PL/pgSQL functions share the bounded routine executor with DO,
-- procedures, and trigger programs.
DROP FUNCTION IF EXISTS plpgsql_function_no_return();
DROP FUNCTION IF EXISTS plpgsql_function_void_implicit();
DROP FUNCTION IF EXISTS plpgsql_function_void_return();
DROP FUNCTION IF EXISTS plpgsql_function_last();
DROP FUNCTION IF EXISTS plpgsql_function_divide(integer);
DROP FUNCTION IF EXISTS plpgsql_function_output(integer);
DROP FUNCTION IF EXISTS plpgsql_function_output_implicit(integer);
DROP FUNCTION IF EXISTS plpgsql_function_configured_actor();
DROP FUNCTION IF EXISTS plpgsql_function_security_actor();
DROP FUNCTION IF EXISTS plpgsql_function_write(integer);
DROP FUNCTION IF EXISTS plpgsql_function_increment(integer);
DROP TABLE IF EXISTS plpgsql_function_rows;
DROP ROLE IF EXISTS plpgsql_function_caller;
DROP ROLE IF EXISTS plpgsql_function_denied;
DROP ROLE IF EXISTS plpgsql_function_owner;

CREATE TABLE plpgsql_function_rows (value integer);
CREATE FUNCTION plpgsql_function_increment(value integer) RETURNS integer
  LANGUAGE plpgsql AS $$
DECLARE
  adjusted integer := value + 1;
BEGIN
  IF adjusted > 41 THEN
    RETURN adjusted;
  END IF;
  RETURN 0;
END
$$;
CREATE FUNCTION plpgsql_function_write(value integer) RETURNS integer
  LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO plpgsql_function_rows VALUES (value);
  RETURN value * 2;
END
$$;
CREATE FUNCTION plpgsql_function_divide(value integer) RETURNS integer
  LANGUAGE plpgsql AS $$
BEGIN
  BEGIN
    RETURN 10 / value;
  EXCEPTION WHEN division_by_zero THEN
    RETURN -1;
  END;
END
$$;
CREATE FUNCTION plpgsql_function_last() RETURNS integer
  LANGUAGE plpgsql AS $$
DECLARE
  observed integer;
BEGIN
  SELECT value INTO observed FROM plpgsql_function_rows;
  RETURN observed;
END
$$;
CREATE FUNCTION plpgsql_function_output(value integer, OUT result integer)
  LANGUAGE plpgsql AS $$ BEGIN result := value + 1; RETURN; END $$;
CREATE FUNCTION plpgsql_function_output_implicit(value integer, OUT result integer)
  LANGUAGE plpgsql AS $$ BEGIN result := value * 2; END $$;
CREATE FUNCTION plpgsql_function_no_return() RETURNS integer
  LANGUAGE plpgsql AS $$ BEGIN NULL; END $$;
CREATE FUNCTION plpgsql_function_void_return() RETURNS void
  LANGUAGE plpgsql AS $$ BEGIN RETURN; END $$;
CREATE FUNCTION plpgsql_function_void_implicit() RETURNS void
  LANGUAGE plpgsql AS $$ BEGIN NULL; END $$;
CREATE ROLE plpgsql_function_owner;
CREATE ROLE plpgsql_function_caller;
CREATE ROLE plpgsql_function_denied;
CREATE FUNCTION plpgsql_function_security_actor() RETURNS text
  LANGUAGE plpgsql SECURITY DEFINER AS $$ BEGIN RETURN current_user; END $$;
ALTER FUNCTION plpgsql_function_security_actor() OWNER TO plpgsql_function_owner;
REVOKE ALL ON FUNCTION plpgsql_function_security_actor() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION plpgsql_function_security_actor() TO plpgsql_function_caller;
CREATE FUNCTION plpgsql_function_configured_actor() RETURNS text
  LANGUAGE plpgsql SET application_name TO 'inside-plpgsql'
  AS $$ BEGIN RETURN current_setting('application_name'); END $$;

SELECT plpgsql_function_increment(41), plpgsql_function_increment(1);
SELECT plpgsql_function_divide(2), plpgsql_function_divide(0);
SELECT plpgsql_function_output(41), plpgsql_function_output_implicit(41);
BEGIN;
SELECT plpgsql_function_write(21);
ROLLBACK;
SELECT count(*) FROM plpgsql_function_rows;
SELECT plpgsql_function_write(7);
SELECT plpgsql_function_last();
SELECT plpgsql_function_no_return();
SELECT plpgsql_function_void_return();
SELECT plpgsql_function_void_implicit();
SET application_name TO 'outside';
SET ROLE plpgsql_function_caller;
SELECT plpgsql_function_security_actor(), plpgsql_function_configured_actor(), current_setting('application_name');
RESET ROLE;
SELECT current_setting('application_name');
SET ROLE plpgsql_function_denied;
SELECT plpgsql_function_security_actor();
RESET ROLE;

DROP FUNCTION plpgsql_function_no_return();
DROP FUNCTION plpgsql_function_void_implicit();
DROP FUNCTION plpgsql_function_void_return();
DROP FUNCTION plpgsql_function_last();
DROP FUNCTION plpgsql_function_divide(integer);
DROP FUNCTION plpgsql_function_output(integer);
DROP FUNCTION plpgsql_function_output_implicit(integer);
DROP FUNCTION plpgsql_function_configured_actor();
DROP FUNCTION plpgsql_function_security_actor();
DROP FUNCTION plpgsql_function_write(integer);
DROP FUNCTION plpgsql_function_increment(integer);
DROP TABLE plpgsql_function_rows;
DROP ROLE plpgsql_function_caller;
DROP ROLE plpgsql_function_denied;
DROP ROLE plpgsql_function_owner;
