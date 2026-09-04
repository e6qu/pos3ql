-- PostgreSQL 18 privilege targets: language, composite type, and routine kind.

CREATE ROLE privilege_target_owner;
CREATE ROLE privilege_target_reader;
CREATE SCHEMA privilege_target_schema;
GRANT USAGE, CREATE ON SCHEMA privilege_target_schema TO privilege_target_owner, privilege_target_reader;

SET ROLE privilege_target_owner;
CREATE TYPE privilege_target_schema.privilege_target_pair AS (left_value integer, right_value integer);
CREATE FUNCTION privilege_target_schema.privilege_target_function() RETURNS integer LANGUAGE sql AS 'SELECT 7';
CREATE PROCEDURE privilege_target_schema.privilege_target_procedure() LANGUAGE sql AS 'SELECT 1';
RESET ROLE;

REVOKE USAGE ON LANGUAGE sql FROM PUBLIC;
GRANT USAGE ON LANGUAGE sql TO privilege_target_reader WITH GRANT OPTION;
GRANT USAGE ON TYPE privilege_target_schema.privilege_target_pair TO privilege_target_reader;
REVOKE EXECUTE ON FUNCTION privilege_target_schema.privilege_target_function() FROM PUBLIC;
REVOKE EXECUTE ON PROCEDURE privilege_target_schema.privilege_target_procedure() FROM PUBLIC;
GRANT EXECUTE ON ALL PROCEDURES IN SCHEMA privilege_target_schema TO privilege_target_reader;

SELECT has_language_privilege('privilege_target_reader', 'sql', 'USAGE'),
       has_language_privilege('privilege_target_reader', 14::oid, 'USAGE'),
       has_type_privilege('privilege_target_reader', 'privilege_target_schema.privilege_target_pair', 'USAGE'),
       has_function_privilege('privilege_target_reader', 'privilege_target_schema.privilege_target_function()', 'EXECUTE'),
       has_function_privilege('privilege_target_reader', 'privilege_target_schema.privilege_target_procedure()', 'EXECUTE');

SET ROLE privilege_target_reader;
CREATE FUNCTION privilege_target_schema.privilege_target_sql_allowed() RETURNS integer LANGUAGE sql AS 'SELECT 9';
CREATE TABLE privilege_target_schema.privilege_target_composite_values (value privilege_target_schema.privilege_target_pair);
RESET ROLE;

GRANT EXECUTE ON ALL ROUTINES IN SCHEMA privilege_target_schema TO privilege_target_reader;
REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA privilege_target_schema FROM privilege_target_reader;
SELECT has_function_privilege('privilege_target_reader', 'privilege_target_schema.privilege_target_function()', 'EXECUTE'),
       has_function_privilege('privilege_target_reader', 'privilege_target_schema.privilege_target_procedure()', 'EXECUTE');
REVOKE EXECUTE ON ALL PROCEDURES IN SCHEMA privilege_target_schema FROM privilege_target_reader;
SELECT has_function_privilege('privilege_target_reader', 'privilege_target_schema.privilege_target_procedure()', 'EXECUTE');

DROP TABLE privilege_target_schema.privilege_target_composite_values;
DROP FUNCTION privilege_target_schema.privilege_target_sql_allowed();
DROP FUNCTION privilege_target_schema.privilege_target_function();
DROP PROCEDURE privilege_target_schema.privilege_target_procedure();
DROP TYPE privilege_target_schema.privilege_target_pair;
REVOKE USAGE ON LANGUAGE sql FROM privilege_target_reader;
GRANT USAGE ON LANGUAGE sql TO PUBLIC;
REVOKE USAGE, CREATE ON SCHEMA privilege_target_schema FROM privilege_target_owner, privilege_target_reader;
DROP SCHEMA privilege_target_schema;
DROP ROLE privilege_target_reader;
DROP ROLE privilege_target_owner;
