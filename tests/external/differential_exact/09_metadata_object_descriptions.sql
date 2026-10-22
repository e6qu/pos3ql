BEGIN;
CREATE FUNCTION description_function() RETURNS integer LANGUAGE SQL AS 'SELECT 1';
COMMENT ON FUNCTION description_function() IS 'function';
CREATE TABLE description_table(id integer);
COMMENT ON TABLE description_table IS 'table';
CREATE TYPE description_type AS ENUM ('ready');
COMMENT ON TYPE description_type IS 'type';
SELECT obj_description('description_function()'::regprocedure),
       obj_description('description_function()'::regprocedure, 'pg_proc');
SELECT obj_description('description_table'::regclass),
       obj_description('description_table'::regclass, 'pg_class');
SELECT obj_description('description_table'::regclass, 'missing_catalog') IS NULL;
SELECT obj_description('description_type'::regtype),
       obj_description('description_type'::regtype, 'pg_type');
SELECT shobj_description('description_function()'::regprocedure, 'pg_proc') IS NULL;
SELECT oid, proargtypes FROM pg_proc WHERE proname = 'obj_description' ORDER BY oid;
ROLLBACK;
