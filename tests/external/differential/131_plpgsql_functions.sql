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
DROP FUNCTION IF EXISTS plpgsql_dynamic_catalog_utility();
DROP FUNCTION IF EXISTS plpgsql_dynamic_catalog_answer();
DROP FUNCTION IF EXISTS plpgsql_dynamic_catalog_publication();
DROP FUNCTION IF EXISTS plpgsql_dynamic_catalog_schema();
DROP FUNCTION IF EXISTS plpgsql_dynamic_catalog_drop_schema();
DROP FUNCTION IF EXISTS plpgsql_dynamic_catalog_lifecycle();
DROP FUNCTION IF EXISTS plpgsql_dynamic_session_commands();
DROP FUNCTION IF EXISTS plpgsql_dynamic_session_reset();
DROP FUNCTION IF EXISTS plpgsql_dynamic_session_prepare();
DROP FUNCTION IF EXISTS plpgsql_dynamic_session_deallocate();
DROP FUNCTION IF EXISTS plpgsql_dynamic_session_portal();
DROP FUNCTION IF EXISTS plpgsql_dynamic_session_lock();
DROP FUNCTION IF EXISTS plpgsql_dynamic_analyze_json();
DROP FUNCTION IF EXISTS plpgsql_dynamic_analyze_compound();
DROP FUNCTION IF EXISTS plpgsql_dynamic_session_constraints();
DROP PUBLICATION IF EXISTS plpgsql_dynamic_catalog_publication;
DROP MATERIALIZED VIEW IF EXISTS plpgsql_dynamic_catalog_materialized;
DROP STATISTICS IF EXISTS plpgsql_dynamic_catalog_stats;
DROP SEQUENCE IF EXISTS plpgsql_dynamic_command_sequence;
DROP SEQUENCE IF EXISTS plpgsql_dynamic_catalog_sequence;
DROP VIEW IF EXISTS plpgsql_dynamic_catalog_view;
DROP TABLE IF EXISTS plpgsql_dynamic_catalog_rows;
DROP TABLE IF EXISTS plpgsql_dynamic_session_rows;
DROP TABLE IF EXISTS plpgsql_dynamic_session_constraints;
DROP SCHEMA IF EXISTS plpgsql_dynamic_catalog_ns CASCADE;
DROP DOMAIN IF EXISTS plpgsql_dynamic_catalog_positive;
DROP TYPE IF EXISTS plpgsql_dynamic_catalog_state;
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
CREATE FUNCTION plpgsql_dynamic_catalog_utility() RETURNS void
  LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'CREATE TABLE plpgsql_dynamic_catalog_rows (id integer PRIMARY KEY, value text)';
  EXECUTE 'CREATE INDEX plpgsql_dynamic_catalog_value_idx ON plpgsql_dynamic_catalog_rows (value)';
  EXECUTE 'CLUSTER plpgsql_dynamic_catalog_rows USING plpgsql_dynamic_catalog_value_idx';
  EXECUTE 'CREATE SEQUENCE plpgsql_dynamic_catalog_sequence START WITH 4';
  EXECUTE 'ALTER SEQUENCE plpgsql_dynamic_catalog_sequence RESTART WITH 9';
  EXECUTE 'CREATE VIEW plpgsql_dynamic_catalog_view AS SELECT id, value FROM plpgsql_dynamic_catalog_rows';
  EXECUTE 'COMMENT ON TABLE plpgsql_dynamic_catalog_rows IS ''dynamic catalog table''';
  EXECUTE 'CREATE TYPE plpgsql_dynamic_catalog_state AS ENUM (''ready'', ''blocked'')';
  EXECUTE 'ALTER TYPE plpgsql_dynamic_catalog_state ADD VALUE ''done''';
  EXECUTE 'CREATE DOMAIN plpgsql_dynamic_catalog_positive AS integer CHECK (VALUE > 0)';
  EXECUTE 'CREATE FUNCTION plpgsql_dynamic_catalog_answer() RETURNS integer LANGUAGE sql AS ''SELECT 43''';
  EXECUTE 'ALTER FUNCTION plpgsql_dynamic_catalog_answer() COST 7';
END
$$;
CREATE FUNCTION plpgsql_dynamic_catalog_publication() RETURNS void
  LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'CREATE PUBLICATION plpgsql_dynamic_catalog_publication
    FOR TABLE plpgsql_dynamic_catalog_rows';
END
$$;
CREATE FUNCTION plpgsql_dynamic_catalog_schema() RETURNS void
  LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'CREATE SCHEMA plpgsql_dynamic_catalog_ns
    CREATE TABLE schema_rows (value integer)
    CREATE VIEW schema_view AS SELECT value FROM schema_rows';
END
$$;
CREATE FUNCTION plpgsql_dynamic_catalog_drop_schema() RETURNS void
  LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'DROP SCHEMA plpgsql_dynamic_catalog_ns CASCADE';
END
$$;
CREATE FUNCTION plpgsql_dynamic_catalog_lifecycle() RETURNS void
  LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'ALTER TABLE plpgsql_dynamic_catalog_rows ENABLE ROW LEVEL SECURITY';
  EXECUTE 'CREATE POLICY plpgsql_dynamic_catalog_policy ON plpgsql_dynamic_catalog_rows FOR SELECT USING (true)';
  EXECUTE 'CREATE STATISTICS plpgsql_dynamic_catalog_stats ON id, value FROM plpgsql_dynamic_catalog_rows';
  EXECUTE 'CREATE MATERIALIZED VIEW plpgsql_dynamic_catalog_materialized AS
    SELECT id, value FROM plpgsql_dynamic_catalog_rows';
  EXECUTE 'REFRESH MATERIALIZED VIEW plpgsql_dynamic_catalog_materialized';
END
$$;
CREATE TABLE plpgsql_dynamic_session_rows (value integer);
CREATE TABLE plpgsql_dynamic_session_constraints
  (value integer UNIQUE DEFERRABLE INITIALLY DEFERRED);
INSERT INTO plpgsql_dynamic_session_rows VALUES (1), (2);
CREATE FUNCTION plpgsql_dynamic_session_commands() RETURNS void
  LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'SET application_name TO ''dynamic-session''';
  EXECUTE 'SET ROLE postgres';
  EXECUTE 'LISTEN plpgsql_dynamic_session';
  EXECUTE 'NOTIFY plpgsql_dynamic_session, ''ready''';
  EXECUTE 'UNLISTEN plpgsql_dynamic_session';
  EXECUTE 'ANALYZE plpgsql_dynamic_session_rows';
  EXECUTE 'CHECKPOINT';
END
$$;
CREATE FUNCTION plpgsql_dynamic_session_reset() RETURNS void
  LANGUAGE plpgsql AS $$ BEGIN EXECUTE 'RESET application_name'; END $$;
CREATE FUNCTION plpgsql_dynamic_session_prepare() RETURNS integer
  LANGUAGE plpgsql AS $$
DECLARE result_value integer;
BEGIN
  EXECUTE 'PREPARE plpgsql_dynamic_session_plan(integer) AS SELECT $1 + 1';
  EXECUTE 'EXECUTE plpgsql_dynamic_session_plan(41)' INTO STRICT result_value;
  EXECUTE 'PREPARE plpgsql_dynamic_session_dml(integer) AS
    INSERT INTO plpgsql_dynamic_session_rows VALUES ($1) RETURNING value';
  EXECUTE 'EXECUTE plpgsql_dynamic_session_dml(3)' INTO STRICT result_value;
  EXECUTE 'DEALLOCATE plpgsql_dynamic_session_dml';
  EXECUTE 'DISCARD PLANS';
  RETURN result_value;
END
$$;
CREATE FUNCTION plpgsql_dynamic_session_deallocate() RETURNS void
  LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'DEALLOCATE plpgsql_dynamic_session_plan';
END
$$;
CREATE FUNCTION plpgsql_dynamic_session_portal() RETURNS integer
  LANGUAGE plpgsql AS $$
DECLARE result_value integer; setting text; plan text; analyze_plan text;
  setting_name text; setting_value text; setting_description text;
BEGIN
  EXECUTE 'SHOW application_name' INTO STRICT setting;
  IF setting <> 'dynamic-session' THEN RAISE EXCEPTION 'SHOW result mismatch'; END IF;
  EXECUTE 'SHOW ALL' INTO setting_name, setting_value, setting_description;
  IF setting_name IS NULL THEN RAISE EXCEPTION 'SHOW ALL result mismatch'; END IF;
  EXECUTE 'EXPLAIN SELECT value FROM plpgsql_dynamic_session_rows' INTO STRICT plan;
  IF plan IS NULL THEN RAISE EXCEPTION 'EXPLAIN result mismatch'; END IF;
  EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF) SELECT value FROM plpgsql_dynamic_session_rows' INTO analyze_plan;
  IF analyze_plan IS NULL OR position('actual time' IN analyze_plan) = 0 THEN
    RAISE EXCEPTION 'EXPLAIN ANALYZE result mismatch';
  END IF;
  EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF) INSERT INTO plpgsql_dynamic_session_rows
    VALUES (4) RETURNING value' INTO analyze_plan;
  IF analyze_plan IS NULL OR position('actual time' IN analyze_plan) = 0 THEN
    RAISE EXCEPTION 'EXPLAIN ANALYZE DML result mismatch';
  END IF;
  EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF) SELECT value FROM plpgsql_dynamic_session_rows';
  EXECUTE 'DECLARE plpgsql_dynamic_session_cursor SCROLL CURSOR FOR
    SELECT value FROM plpgsql_dynamic_session_rows ORDER BY value';
  EXECUTE 'MOVE FORWARD 1 FROM plpgsql_dynamic_session_cursor';
  EXECUTE 'FETCH NEXT FROM plpgsql_dynamic_session_cursor' INTO STRICT result_value;
  EXECUTE 'CLOSE plpgsql_dynamic_session_cursor';
  RETURN result_value;
END
$$;
CREATE FUNCTION plpgsql_dynamic_session_lock() RETURNS void
  LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'LOCK TABLE plpgsql_dynamic_session_rows IN SHARE MODE';
END
$$;
CREATE FUNCTION plpgsql_dynamic_analyze_json() RETURNS void
  LANGUAGE plpgsql AS $$
DECLARE plan text;
BEGIN
  EXECUTE 'EXPLAIN (ANALYZE, FORMAT JSON) SELECT value FROM plpgsql_dynamic_session_rows'
    INTO STRICT plan;
  IF plan IS NULL THEN RAISE EXCEPTION 'EXPLAIN ANALYZE JSON result mismatch'; END IF;
END
$$;
CREATE FUNCTION plpgsql_dynamic_analyze_compound() RETURNS void
  LANGUAGE plpgsql AS $$
DECLARE plan text;
BEGIN
  EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF) SELECT value FROM plpgsql_dynamic_session_rows
    UNION ALL SELECT value FROM plpgsql_dynamic_session_rows' INTO plan;
  IF plan IS NULL OR position('actual time' IN plan) = 0 THEN
    RAISE EXCEPTION 'EXPLAIN ANALYZE set-query result mismatch';
  END IF;
  EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF) WITH inserted AS
    (INSERT INTO plpgsql_dynamic_session_rows VALUES (5) RETURNING value)
    SELECT value FROM inserted' INTO plan;
  IF plan IS NULL OR position('actual time' IN plan) = 0 THEN
    RAISE EXCEPTION 'EXPLAIN ANALYZE data-modifying WITH result mismatch';
  END IF;
END
$$;
CREATE FUNCTION plpgsql_dynamic_session_constraints() RETURNS void
  LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'SET CONSTRAINTS ALL IMMEDIATE';
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
SELECT plpgsql_dynamic_catalog_utility();
SELECT plpgsql_dynamic_catalog_publication();
SELECT plpgsql_dynamic_catalog_schema();
INSERT INTO plpgsql_dynamic_catalog_ns.schema_rows VALUES (11);
SELECT * FROM plpgsql_dynamic_catalog_ns.schema_view;
INSERT INTO plpgsql_dynamic_catalog_rows VALUES (nextval('plpgsql_dynamic_catalog_sequence'), 'nine');
SELECT plpgsql_dynamic_catalog_lifecycle();
SELECT plpgsql_dynamic_session_commands();
SELECT current_setting('application_name');
SELECT reltuples::integer FROM pg_class WHERE relname = 'plpgsql_dynamic_session_rows';
SELECT plpgsql_dynamic_session_reset();
SELECT plpgsql_dynamic_session_prepare();
EXECUTE plpgsql_dynamic_session_plan(41);
BEGIN;
SELECT plpgsql_dynamic_session_portal();
COMMIT;
SELECT plpgsql_dynamic_analyze_compound();
SELECT count(*) FROM plpgsql_dynamic_session_rows;
SELECT plpgsql_dynamic_session_deallocate();
SELECT plpgsql_dynamic_analyze_json();
BEGIN;
SELECT plpgsql_dynamic_session_lock();
COMMIT;
BEGIN;
SELECT plpgsql_dynamic_session_constraints();
COMMIT;
SELECT * FROM plpgsql_dynamic_catalog_view;
SELECT * FROM plpgsql_dynamic_catalog_materialized;
SELECT obj_description('plpgsql_dynamic_catalog_rows'::regclass, 'pg_class');
SELECT enumlabel FROM pg_enum
 WHERE enumtypid = 'plpgsql_dynamic_catalog_state'::regtype
 ORDER BY enumsortorder;
SELECT 7::plpgsql_dynamic_catalog_positive;
SELECT plpgsql_dynamic_catalog_answer();
SELECT pubname FROM pg_publication
 WHERE pubname = 'plpgsql_dynamic_catalog_publication';
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

SELECT lo_create(92701::oid);
CREATE ROLE plpgsql_dynamic_admin_owner;
CREATE FUNCTION plpgsql_dynamic_administration() RETURNS void
  LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'CREATE SCHEMA plpgsql_dynamic_admin_schema';
  EXECUTE 'ALTER SCHEMA plpgsql_dynamic_admin_schema RENAME TO plpgsql_dynamic_admin_schema_moved';
  EXECUTE 'CREATE TABLE plpgsql_dynamic_admin_rows (id integer)';
  EXECUTE 'ALTER TABLE plpgsql_dynamic_admin_rows OWNER TO plpgsql_dynamic_admin_owner';
  EXECUTE 'ALTER LARGE OBJECT 92701 OWNER TO plpgsql_dynamic_admin_owner';
  EXECUTE 'ALTER ROLE plpgsql_dynamic_admin_owner SET application_name TO ''dynamic-admin''';
  EXECUTE 'GRANT SET ON PARAMETER event_triggers TO plpgsql_dynamic_admin_owner';
END
$$;
CREATE FUNCTION plpgsql_dynamic_administration_revoke() RETURNS void
  LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'REVOKE SET ON PARAMETER event_triggers FROM plpgsql_dynamic_admin_owner';
END
$$;
SELECT plpgsql_dynamic_administration();
SELECT nspname FROM pg_namespace
 WHERE nspname = 'plpgsql_dynamic_admin_schema_moved';
SELECT relowner::regrole::text FROM pg_class
 WHERE relname = 'plpgsql_dynamic_admin_rows';
SELECT lomowner::regrole::text FROM pg_largeobject_metadata WHERE oid = 92701;
SELECT has_parameter_privilege('plpgsql_dynamic_admin_owner', 'event_triggers', 'SET');
SELECT plpgsql_dynamic_administration_revoke();
SELECT has_parameter_privilege('plpgsql_dynamic_admin_owner', 'event_triggers', 'SET');

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
DROP SEQUENCE plpgsql_dynamic_catalog_sequence;
DROP TRIGGER plpgsql_dynamic_dml_rows_before_insert ON plpgsql_dynamic_dml_rows;
DROP FUNCTION plpgsql_dynamic_dml_trigger();
DROP PROCEDURE plpgsql_dynamic_dml_procedure(integer);
DROP FUNCTION plpgsql_dynamic_dml(integer);
DROP FUNCTION plpgsql_dynamic_utility();
DROP FUNCTION plpgsql_dynamic_catalog_utility();
DROP FUNCTION plpgsql_dynamic_catalog_answer();
DROP FUNCTION plpgsql_dynamic_catalog_publication();
SELECT plpgsql_dynamic_catalog_drop_schema();
DROP FUNCTION plpgsql_dynamic_catalog_drop_schema();
DROP FUNCTION plpgsql_dynamic_catalog_schema();
DROP FUNCTION plpgsql_dynamic_catalog_lifecycle();
DROP FUNCTION plpgsql_dynamic_session_commands();
DROP FUNCTION plpgsql_dynamic_session_reset();
DROP FUNCTION plpgsql_dynamic_session_prepare();
DROP FUNCTION plpgsql_dynamic_session_deallocate();
DROP FUNCTION plpgsql_dynamic_session_portal();
DROP FUNCTION plpgsql_dynamic_session_lock();
DROP FUNCTION plpgsql_dynamic_analyze_json();
DROP FUNCTION plpgsql_dynamic_session_constraints();
DROP PUBLICATION plpgsql_dynamic_catalog_publication;
DROP MATERIALIZED VIEW plpgsql_dynamic_catalog_materialized;
DROP STATISTICS plpgsql_dynamic_catalog_stats;
DROP POLICY plpgsql_dynamic_catalog_policy ON plpgsql_dynamic_catalog_rows;
DROP TABLE plpgsql_function_rows;
DROP TABLE plpgsql_dynamic_rows;
DROP TABLE plpgsql_dynamic_dml_rows;
DROP TABLE plpgsql_dynamic_dml_audit;
DROP TABLE plpgsql_dynamic_utility_rows;
DROP VIEW plpgsql_dynamic_catalog_view;
DROP TABLE plpgsql_dynamic_catalog_rows;
DROP TABLE plpgsql_dynamic_session_rows;
DROP TABLE plpgsql_dynamic_session_constraints;
DROP DOMAIN plpgsql_dynamic_catalog_positive;
DROP TYPE plpgsql_dynamic_catalog_state;
DROP FUNCTION plpgsql_dynamic_administration_revoke();
DROP FUNCTION plpgsql_dynamic_administration();
DROP TABLE plpgsql_dynamic_admin_rows;
DROP SCHEMA plpgsql_dynamic_admin_schema_moved;
SELECT lo_unlink(92701::oid);
DROP ROLE plpgsql_dynamic_admin_owner;
DROP ROLE plpgsql_function_caller;
DROP ROLE plpgsql_function_denied;
DROP ROLE plpgsql_function_owner;
