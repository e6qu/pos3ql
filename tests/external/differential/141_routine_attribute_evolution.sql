-- Routine attributes, configuration, ownership, and ACLs have one catalog
-- identity through all ALTER command spellings.

CREATE ROLE routine_attribute_owner;
CREATE ROLE routine_attribute_reader;
CREATE SCHEMA routine_attribute_schema;
GRANT USAGE, CREATE ON SCHEMA routine_attribute_schema
  TO routine_attribute_owner, routine_attribute_reader;

CREATE FUNCTION routine_attribute_schema.scalar_value(integer) RETURNS integer
  LANGUAGE SQL VOLATILE CALLED ON NULL INPUT SECURITY INVOKER
  PARALLEL UNSAFE COST 100 RETURN $1 + 1;
CREATE PROCEDURE routine_attribute_schema.record_value(integer)
  LANGUAGE SQL SECURITY INVOKER AS 'SELECT $1 + 1';

ALTER FUNCTION routine_attribute_schema.scalar_value(integer)
  IMMUTABLE STRICT LEAKPROOF SECURITY DEFINER PARALLEL SAFE COST 17 ROWS 4;
ALTER FUNCTION routine_attribute_schema.scalar_value(integer)
  SET search_path TO public;
ALTER ROUTINE routine_attribute_schema.scalar_value(integer)
  RESET search_path;
ALTER PROCEDURE routine_attribute_schema.record_value(integer)
  SECURITY DEFINER SET statement_timeout TO '3s';
ALTER ROUTINE routine_attribute_schema.scalar_value(integer)
  OWNER TO routine_attribute_owner;
ALTER PROCEDURE routine_attribute_schema.record_value(integer)
  OWNER TO routine_attribute_owner;

REVOKE EXECUTE ON FUNCTION routine_attribute_schema.scalar_value(integer) FROM PUBLIC;
REVOKE EXECUTE ON PROCEDURE routine_attribute_schema.record_value(integer) FROM PUBLIC;
GRANT EXECUTE ON ROUTINE routine_attribute_schema.scalar_value(integer),
  routine_attribute_schema.record_value(integer) TO routine_attribute_reader;

SELECT procedure.proname, procedure.provolatile, procedure.proisstrict,
       procedure.proleakproof, procedure.prosecdef, procedure.proparallel,
       procedure.procost, procedure.prorows, procedure.proconfig,
       pg_get_userbyid(procedure.proowner)
  FROM pg_proc procedure
  JOIN pg_namespace namespace ON namespace.oid = procedure.pronamespace
 WHERE namespace.nspname = 'routine_attribute_schema'
 ORDER BY procedure.proname;
SELECT routine_attribute_schema.scalar_value(4);

SET ROLE routine_attribute_reader;
SELECT routine_attribute_schema.scalar_value(5);
CALL routine_attribute_schema.record_value(6);
RESET ROLE;

DROP FUNCTION routine_attribute_schema.scalar_value(integer);
DROP PROCEDURE routine_attribute_schema.record_value(integer);
REVOKE USAGE, CREATE ON SCHEMA routine_attribute_schema
  FROM routine_attribute_owner, routine_attribute_reader;
DROP SCHEMA routine_attribute_schema;
DROP ROLE routine_attribute_reader;
DROP ROLE routine_attribute_owner;
