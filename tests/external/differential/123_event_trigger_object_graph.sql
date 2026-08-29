DROP EVENT TRIGGER IF EXISTS object_graph_ddl;
DROP EVENT TRIGGER IF EXISTS object_graph_drop;
DROP TABLE IF EXISTS object_graph_commands, object_graph_drops CASCADE;
DROP FUNCTION IF EXISTS object_graph_capture_ddl() CASCADE;
DROP FUNCTION IF EXISTS object_graph_capture_drop() CASCADE;
DROP SCHEMA IF EXISTS object_graph CASCADE;

CREATE TABLE object_graph_commands
  (position serial, tag text, kind text, schema_name text, identity text,
   in_extension boolean);
CREATE TABLE object_graph_drops
  (position serial, original boolean, normal boolean, kind text,
   schema_name text, identity text, names text[], args text[]);
CREATE FUNCTION object_graph_capture_ddl() RETURNS event_trigger LANGUAGE plpgsql AS
  'BEGIN
     INSERT INTO object_graph_commands(tag,kind,schema_name,identity,in_extension)
     SELECT command_tag,object_type,schema_name,object_identity,in_extension
       FROM pg_event_trigger_ddl_commands();
     RETURN;
   END';
CREATE FUNCTION object_graph_capture_drop() RETURNS event_trigger LANGUAGE plpgsql AS
  'BEGIN
     INSERT INTO object_graph_drops
       (original,normal,kind,schema_name,identity,names,args)
     SELECT original,normal,object_type,schema_name,object_identity,
            address_names,address_args
       FROM pg_event_trigger_dropped_objects();
     RETURN;
   END';
CREATE EVENT TRIGGER object_graph_ddl ON ddl_command_end
  EXECUTE FUNCTION object_graph_capture_ddl();
CREATE EVENT TRIGGER object_graph_drop ON sql_drop
  EXECUTE FUNCTION object_graph_capture_drop();

CREATE SCHEMA object_graph;
CREATE TYPE object_graph.mood AS ENUM ('calm', 'busy');
CREATE TYPE object_graph.pair AS (left_value integer, right_value text);
CREATE DOMAIN object_graph.positive AS integer
  CONSTRAINT positive_check CHECK (VALUE > 0);
CREATE TABLE object_graph.base(id integer);
CREATE VIEW object_graph.base_view AS SELECT id FROM object_graph.base;
CREATE FUNCTION object_graph.identity(value integer, labels text[])
RETURNS integer LANGUAGE SQL AS 'SELECT value';
CREATE FUNCTION object_graph.overloaded(value integer)
RETURNS integer LANGUAGE SQL RETURN value;
CREATE FUNCTION object_graph.overloaded(value text)
RETURNS integer LANGUAGE SQL RETURN object_graph.overloaded(1);

DROP VIEW object_graph.base_view;
DROP DOMAIN object_graph.positive;
DROP TYPE object_graph.pair;
DROP TYPE object_graph.mood;
DROP FUNCTION object_graph.identity(integer, text[]);
DROP FUNCTION object_graph.overloaded(integer) CASCADE;
CREATE VIEW object_graph.schema_view AS SELECT id FROM object_graph.base;
DROP SCHEMA object_graph CASCADE;

SELECT tag,kind,schema_name,identity,in_extension
FROM object_graph_commands ORDER BY position;
SELECT original,normal,kind,schema_name,identity,names::text,args::text
FROM object_graph_drops ORDER BY position;

DROP EVENT TRIGGER object_graph_ddl;
DROP EVENT TRIGGER object_graph_drop;
DROP TABLE object_graph_commands, object_graph_drops;
DROP FUNCTION object_graph_capture_ddl(), object_graph_capture_drop();
