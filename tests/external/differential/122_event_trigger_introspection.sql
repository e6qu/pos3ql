DROP EVENT TRIGGER IF EXISTS introspection_ddl;
DROP EVENT TRIGGER IF EXISTS introspection_drop;
DROP TABLE IF EXISTS introspection_target, introspection_ddl_log,
  introspection_drop_log CASCADE;
DROP FUNCTION IF EXISTS introspection_capture_ddl() CASCADE;
DROP FUNCTION IF EXISTS introspection_capture_drop() CASCADE;

CREATE TABLE introspection_ddl_log
  (position serial, classid oid, objid oid, objsubid integer,
   tag text, kind text, schema_name text, identity text,
   in_extension boolean, command_present boolean);
CREATE TABLE introspection_drop_log
  (position serial, classid oid, objsubid integer, original boolean,
  normal boolean, is_temporary boolean, kind text, schema_name text,
   object_name text, identity text, names text[], args text[]);
CREATE FUNCTION introspection_capture_ddl() RETURNS event_trigger LANGUAGE plpgsql AS
  'BEGIN
     INSERT INTO introspection_ddl_log
       (classid,objid,objsubid,tag,kind,schema_name,identity,
        in_extension,command_present)
     SELECT classid,objid,objsubid,command_tag,object_type,schema_name,
            object_identity,in_extension,command IS NOT NULL
       FROM pg_event_trigger_ddl_commands();
     RETURN;
   END';
CREATE FUNCTION introspection_capture_drop() RETURNS event_trigger LANGUAGE plpgsql AS
  'BEGIN
     INSERT INTO introspection_drop_log
       (classid,objsubid,original,normal,is_temporary,kind,schema_name,
        object_name,identity,names,args)
     SELECT classid,objsubid,original,normal,is_temporary,object_type,
            schema_name,object_name,object_identity,address_names,address_args
       FROM pg_event_trigger_dropped_objects();
     RETURN;
   END';
CREATE EVENT TRIGGER introspection_ddl ON ddl_command_end
  EXECUTE FUNCTION introspection_capture_ddl();
CREATE EVENT TRIGGER introspection_drop ON sql_drop
  EXECUTE FUNCTION introspection_capture_drop();

CREATE TABLE introspection_target
  (id integer PRIMARY KEY, value integer NOT NULL,
   label text, CONSTRAINT introspection_check CHECK (value > 0),
   CONSTRAINT introspection_label_key UNIQUE (label));
ALTER TABLE introspection_target DROP COLUMN label CASCADE;
ALTER TABLE introspection_target DROP CONSTRAINT introspection_check;
ALTER TABLE introspection_target ALTER COLUMN value DROP NOT NULL;
COMMENT ON TABLE introspection_target IS 'table';
COMMENT ON COLUMN introspection_target.id IS 'column';
ALTER TABLE introspection_target OWNER TO postgres;
GRANT SELECT ON introspection_target TO PUBLIC;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO PUBLIC;
DROP TABLE introspection_target;

SELECT classid::regclass::text, objid IS NULL, objsubid, tag, kind,
       schema_name, identity, in_extension, command_present
FROM introspection_ddl_log ORDER BY position;
SELECT classid::regclass::text, objsubid, original, normal, is_temporary,
       kind, schema_name,
       CASE WHEN schema_name = 'pg_toast' THEN kind ELSE object_name END,
       CASE WHEN schema_name = 'pg_toast' THEN kind ELSE identity END,
       CASE WHEN schema_name = 'pg_toast' THEN ARRAY['pg_toast', kind]
            ELSE names END::text,
       args::text
FROM introspection_drop_log ORDER BY position;

DROP EVENT TRIGGER introspection_ddl;
DROP EVENT TRIGGER introspection_drop;
DROP TABLE introspection_ddl_log, introspection_drop_log;
DROP FUNCTION introspection_capture_ddl(), introspection_capture_drop();
