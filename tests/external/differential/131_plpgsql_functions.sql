-- Scalar PL/pgSQL functions share the bounded routine executor with DO,
-- procedures, and trigger programs.
DROP FUNCTION IF EXISTS plpgsql_function_no_return();
DROP FUNCTION IF EXISTS plpgsql_function_void_implicit();
DROP FUNCTION IF EXISTS plpgsql_function_void_return();
DROP FUNCTION IF EXISTS plpgsql_function_last();
DROP FUNCTION IF EXISTS plpgsql_function_divide(integer);
DROP FUNCTION IF EXISTS plpgsql_function_output(integer);
DROP FUNCTION IF EXISTS plpgsql_function_output_implicit(integer);
DROP FUNCTION IF EXISTS plpgsql_function_pair(integer);
DROP FUNCTION IF EXISTS plpgsql_function_series(integer);
DROP FUNCTION IF EXISTS plpgsql_function_table_series(integer);
DROP FUNCTION IF EXISTS plpgsql_function_configured_actor();
DROP FUNCTION IF EXISTS plpgsql_function_security_actor();
DROP FUNCTION IF EXISTS plpgsql_function_write(integer);
DROP FUNCTION IF EXISTS plpgsql_function_increment(integer);
DROP FUNCTION IF EXISTS plpgsql_dynamic_scalar(integer);
DROP FUNCTION IF EXISTS plpgsql_dynamic_series(integer);
DROP FUNCTION IF EXISTS plpgsql_dynamic_loop(integer);
DROP FUNCTION IF EXISTS plpgsql_dynamic_record_loop(integer);
DROP FUNCTION IF EXISTS plpgsql_dynamic_command_once();
DROP FUNCTION IF EXISTS plpgsql_dynamic_trigger();
DROP FUNCTION IF EXISTS plpgsql_dynamic_dml(integer);
DROP PROCEDURE IF EXISTS plpgsql_dynamic_dml_procedure(integer);
DROP FUNCTION IF EXISTS plpgsql_dynamic_dml_trigger();
DROP FUNCTION IF EXISTS plpgsql_dynamic_utility();
DROP SEQUENCE IF EXISTS plpgsql_dynamic_command_sequence;
DROP TABLE IF EXISTS plpgsql_function_rows;
DROP TABLE IF EXISTS plpgsql_dynamic_rows;
DROP TABLE IF EXISTS plpgsql_dynamic_dml_rows;
DROP TABLE IF EXISTS plpgsql_dynamic_dml_audit;
DROP TABLE IF EXISTS plpgsql_dynamic_utility_rows;
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
CREATE FUNCTION plpgsql_function_pair(value integer, OUT next_value integer, OUT label text)
  LANGUAGE plpgsql AS $$
BEGIN
  next_value := value + 1;
  label := 'value:' || value;
  RETURN;
END
$$;
CREATE FUNCTION plpgsql_function_series(limit_value integer) RETURNS SETOF integer
  LANGUAGE plpgsql AS $$
DECLARE
  item integer;
BEGIN
  FOR item IN 1..limit_value LOOP
    RETURN NEXT item * 2;
  END LOOP;
  RETURN;
END
$$;
CREATE FUNCTION plpgsql_function_table_series(limit_value integer)
  RETURNS TABLE (value integer, label text)
  LANGUAGE plpgsql AS $$
BEGIN
  FOR value IN 1..limit_value LOOP
    label := 'item:' || value;
    RETURN NEXT;
  END LOOP;
  RETURN QUERY SELECT limit_value + 1, 'tail';
END
$$;
CREATE FUNCTION plpgsql_dynamic_scalar(input_value integer) RETURNS integer
  LANGUAGE plpgsql AS $$
DECLARE
  result_value integer;
BEGIN
  EXECUTE 'SELECT $1::integer * 3' INTO STRICT result_value USING input_value;
  RETURN result_value;
END
$$;
CREATE FUNCTION plpgsql_dynamic_series(input_value integer) RETURNS SETOF integer
  LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY EXECUTE 'VALUES ($1::integer), ($1::integer + 1)' USING input_value;
END
$$;
CREATE FUNCTION plpgsql_dynamic_loop(input_value integer) RETURNS integer
  LANGUAGE plpgsql AS $$
DECLARE
  item integer;
  total integer := 0;
BEGIN
  FOR item IN EXECUTE 'VALUES ($1::integer), ($1::integer + 1)' USING input_value LOOP
    total := total + item;
  END LOOP;
  RETURN total;
END
$$;
CREATE FUNCTION plpgsql_dynamic_record_loop(input_value integer) RETURNS integer
  LANGUAGE plpgsql AS $$
DECLARE
  item record;
BEGIN
  FOR item IN EXECUTE 'SELECT $1::integer AS value, $1::integer + 1 AS next_value' USING input_value LOOP
    RETURN item.value + item.next_value;
  END LOOP;
  RETURN 0;
END
$$;
CREATE SEQUENCE plpgsql_dynamic_command_sequence;
CREATE FUNCTION plpgsql_dynamic_command_once() RETURNS integer
  LANGUAGE plpgsql AS $$
DECLARE
  item integer;
BEGIN
  FOR item IN EXECUTE 'VALUES (' || nextval('plpgsql_dynamic_command_sequence') || ')' LOOP
    RETURN item;
  END LOOP;
  RETURN 0;
END
$$;
CREATE TABLE plpgsql_dynamic_rows (value integer);
CREATE FUNCTION plpgsql_dynamic_trigger() RETURNS trigger
  LANGUAGE plpgsql AS $$
DECLARE
  adjusted integer;
BEGIN
  EXECUTE 'SELECT $1::integer + 1' INTO adjusted USING NEW.value;
  NEW.value := adjusted;
  RETURN NEW;
END
$$;
CREATE TRIGGER plpgsql_dynamic_rows_before_insert
  BEFORE INSERT ON plpgsql_dynamic_rows FOR EACH ROW EXECUTE FUNCTION plpgsql_dynamic_trigger();
CREATE TABLE plpgsql_dynamic_dml_rows (value integer PRIMARY KEY);
CREATE TABLE plpgsql_dynamic_dml_audit (value integer);
CREATE FUNCTION plpgsql_dynamic_dml(input_value integer) RETURNS integer
  LANGUAGE plpgsql AS $$
DECLARE
  returned_value integer;
  changed bigint;
BEGIN
  EXECUTE 'INSERT INTO plpgsql_dynamic_dml_rows VALUES ($1) RETURNING value'
    INTO STRICT returned_value USING input_value;
  GET DIAGNOSTICS changed = ROW_COUNT;
  IF changed <> 1 THEN RAISE EXCEPTION 'dynamic INSERT count mismatch'; END IF;
  EXECUTE 'UPDATE plpgsql_dynamic_dml_rows SET value = $1 + 1 WHERE value = $1 RETURNING value'
    INTO STRICT returned_value USING returned_value;
  EXECUTE 'DELETE FROM plpgsql_dynamic_dml_rows WHERE value = $1 RETURNING value'
    INTO STRICT returned_value USING returned_value;
  EXECUTE 'UPDATE plpgsql_dynamic_dml_rows SET value = 0 WHERE value = $1' USING returned_value;
  GET DIAGNOSTICS changed = ROW_COUNT;
  IF changed <> 0 THEN
    RAISE EXCEPTION 'dynamic no-row UPDATE count mismatch';
  END IF;
  RETURN returned_value;
END
$$;
CREATE PROCEDURE plpgsql_dynamic_dml_procedure(input_value integer)
  LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'INSERT INTO plpgsql_dynamic_dml_audit VALUES ($1)' USING input_value;
END
$$;
CREATE FUNCTION plpgsql_dynamic_dml_trigger() RETURNS trigger
  LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'INSERT INTO plpgsql_dynamic_dml_audit VALUES ($1)' USING NEW.value;
  RETURN NEW;
END
$$;
CREATE TRIGGER plpgsql_dynamic_dml_rows_before_insert
  BEFORE INSERT ON plpgsql_dynamic_dml_rows FOR EACH ROW EXECUTE FUNCTION plpgsql_dynamic_dml_trigger();
CREATE FUNCTION plpgsql_dynamic_utility() RETURNS void
  LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'CREATE TABLE plpgsql_dynamic_utility_rows (value integer)';
  EXECUTE 'INSERT INTO plpgsql_dynamic_utility_rows VALUES (89)';
END
$$;
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
SELECT (plpgsql_function_pair(7)).next_value, (plpgsql_function_pair(7)).label;
SELECT * FROM plpgsql_function_series(3);
SELECT * FROM plpgsql_function_table_series(2);
SELECT plpgsql_dynamic_scalar(14), plpgsql_dynamic_loop(14), plpgsql_dynamic_record_loop(14);
SELECT * FROM plpgsql_dynamic_series(14);
SELECT plpgsql_dynamic_command_once(), plpgsql_dynamic_command_once();
INSERT INTO plpgsql_dynamic_rows VALUES (14);
SELECT * FROM plpgsql_dynamic_rows;
SELECT plpgsql_dynamic_dml(14);
CALL plpgsql_dynamic_dml_procedure(21);
INSERT INTO plpgsql_dynamic_dml_rows VALUES (34);
DO $$ BEGIN EXECUTE 'INSERT INTO plpgsql_dynamic_dml_audit VALUES ($1)' USING 55; END $$;
SELECT plpgsql_dynamic_utility();
SELECT * FROM plpgsql_dynamic_dml_rows;
SELECT * FROM plpgsql_dynamic_dml_audit ORDER BY value;
SELECT * FROM plpgsql_dynamic_utility_rows;
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
DROP FUNCTION plpgsql_function_table_series(integer);
DROP FUNCTION plpgsql_function_series(integer);
DROP FUNCTION plpgsql_function_pair(integer);
DROP FUNCTION plpgsql_function_configured_actor();
DROP FUNCTION plpgsql_function_security_actor();
DROP FUNCTION plpgsql_function_write(integer);
DROP FUNCTION plpgsql_function_increment(integer);
DROP TRIGGER plpgsql_dynamic_rows_before_insert ON plpgsql_dynamic_rows;
DROP FUNCTION plpgsql_dynamic_trigger();
DROP FUNCTION plpgsql_dynamic_loop(integer);
DROP FUNCTION plpgsql_dynamic_record_loop(integer);
DROP FUNCTION plpgsql_dynamic_series(integer);
DROP FUNCTION plpgsql_dynamic_scalar(integer);
DROP FUNCTION plpgsql_dynamic_command_once();
DROP SEQUENCE plpgsql_dynamic_command_sequence;
DROP TRIGGER plpgsql_dynamic_dml_rows_before_insert ON plpgsql_dynamic_dml_rows;
DROP FUNCTION plpgsql_dynamic_dml_trigger();
DROP PROCEDURE plpgsql_dynamic_dml_procedure(integer);
DROP FUNCTION plpgsql_dynamic_dml(integer);
DROP FUNCTION plpgsql_dynamic_utility();
DROP TABLE plpgsql_function_rows;
DROP TABLE plpgsql_dynamic_rows;
DROP TABLE plpgsql_dynamic_dml_rows;
DROP TABLE plpgsql_dynamic_dml_audit;
DROP TABLE plpgsql_dynamic_utility_rows;
DROP ROLE plpgsql_function_caller;
DROP ROLE plpgsql_function_denied;
DROP ROLE plpgsql_function_owner;
