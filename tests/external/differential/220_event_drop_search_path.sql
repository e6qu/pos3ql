CREATE SCHEMA drop_path_a;
CREATE SCHEMA drop_path_b;
CREATE TABLE public.drop_path_log
  (kind text, schema_name text, name text, identity text, original boolean);
CREATE FUNCTION public.capture_drop_path() RETURNS event_trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO public.drop_path_log
     SELECT object_type, schema_name, coalesce(object_name, address_names[2]),
            object_identity, original
       FROM pg_event_trigger_dropped_objects(); RETURN; END';
CREATE EVENT TRIGGER capture_drop_path ON sql_drop
  EXECUTE FUNCTION public.capture_drop_path();

CREATE TABLE drop_path_a.shared_base(id integer PRIMARY KEY);
CREATE TABLE drop_path_b.shared_base(id integer PRIMARY KEY);
CREATE TABLE drop_path_b.retained_child
  (id integer CONSTRAINT retained_fk REFERENCES drop_path_b.shared_base(id));
CREATE VIEW drop_path_b.shared_view AS SELECT id FROM drop_path_a.shared_base;
CREATE FUNCTION drop_path_a.shared_function(value integer) RETURNS integer
  LANGUAGE SQL RETURN value;
CREATE FUNCTION drop_path_b.shared_function(value integer) RETURNS integer
  LANGUAGE SQL RETURN value + 1;
CREATE FUNCTION drop_path_b.shared_function(value text) RETURNS integer
  LANGUAGE SQL RETURN drop_path_a.shared_function(1);
CREATE DOMAIN drop_path_a.shared_domain AS integer;
CREATE DOMAIN drop_path_b.shared_domain AS integer;
CREATE FUNCTION drop_path_a.operator_body(integer, integer) RETURNS integer
  LANGUAGE SQL IMMUTABLE RETURN $1 + $2;
CREATE FUNCTION drop_path_b.operator_body(integer, integer) RETURNS integer
  LANGUAGE SQL IMMUTABLE RETURN $1 + $2 + 100;
CREATE OPERATOR drop_path_a.##
  (LEFTARG = integer, RIGHTARG = integer, FUNCTION = drop_path_a.operator_body);
CREATE OPERATOR drop_path_b.##
  (LEFTARG = integer, RIGHTARG = integer, FUNCTION = drop_path_b.operator_body);
CREATE TABLE drop_path_a.index_owner(id integer);
CREATE TABLE drop_path_b.index_owner(id integer);
CREATE INDEX shared_index ON drop_path_a.index_owner(id);
CREATE INDEX shared_index ON drop_path_b.index_owner(id);

SET search_path = drop_path_a, drop_path_b, public;
DROP TABLE shared_base CASCADE;
DROP FUNCTION shared_function(integer) CASCADE;
DROP DOMAIN shared_domain;
DROP OPERATOR ## (integer, integer);
DROP INDEX shared_index;
SELECT kind, schema_name, name, original FROM public.drop_path_log
 WHERE (kind = 'type' AND name = 'shared_domain')
    OR (kind IN ('table', 'view', 'function', 'operator', 'index')
   AND name IN ('shared_base', 'shared_view', 'shared_function',
                '##', 'shared_index'))
 ORDER BY kind, schema_name, name;
SELECT count(*) FROM public.drop_path_log WHERE identity LIKE '%retained_fk%';
INSERT INTO drop_path_b.shared_base VALUES (9);
INSERT INTO drop_path_b.retained_child VALUES (9);
SELECT drop_path_b.shared_function(4), 4 OPERATOR(drop_path_b.##) 2;
SELECT count(*) FROM pg_type
 WHERE typname = 'shared_domain' AND typnamespace = 'drop_path_b'::regnamespace;
SELECT count(*) FROM pg_class
 WHERE relname = 'shared_index' AND relnamespace = 'drop_path_b'::regnamespace;
RESET search_path;
DROP EVENT TRIGGER capture_drop_path;
DROP SCHEMA drop_path_a, drop_path_b CASCADE;
DROP FUNCTION public.capture_drop_path();
DROP TABLE public.drop_path_log;
