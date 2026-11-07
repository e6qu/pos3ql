CREATE SCHEMA trigger_context_schema;
CREATE TABLE trigger_context_schema.trigger_context_target (id integer PRIMARY KEY);
CREATE TABLE trigger_context_audit (
  name text, timing text, level text, operation text, relid integer,
  table_name text, table_schema text, nargs integer, first_argument text, second_argument text,
  relation_name text, context_types text
);
CREATE FUNCTION trigger_context_function() RETURNS trigger LANGUAGE plpgsql AS
  $$BEGIN
     INSERT INTO trigger_context_audit VALUES
       (TG_NAME, TG_WHEN, TG_LEVEL, TG_OP, TG_RELID, TG_TABLE_NAME,
        TG_TABLE_SCHEMA, TG_NARGS, TG_ARGV[0], TG_ARGV[1], TG_RELNAME,
        pg_typeof(TG_NAME)::text || ':' || pg_typeof(TG_RELID)::text);
     RETURN NEW;
   END$$;
CREATE TABLE trigger_context_conflict (value text PRIMARY KEY);
INSERT INTO trigger_context_conflict VALUES ('first');
CREATE FUNCTION trigger_context_nested_function() RETURNS trigger LANGUAGE plpgsql AS
  $$BEGIN
     INSERT INTO trigger_context_conflict VALUES (TG_ARGV[0])
       ON CONFLICT (value) DO UPDATE SET value = TG_ARGV[1];
     RETURN NEW;
   END$$;
CREATE TRIGGER trigger_context_name BEFORE INSERT
  ON trigger_context_schema.trigger_context_target FOR EACH ROW
  EXECUTE FUNCTION trigger_context_function('first', 'second');
CREATE TRIGGER trigger_context_nested_name AFTER INSERT
  ON trigger_context_schema.trigger_context_target FOR EACH ROW
  EXECUTE FUNCTION trigger_context_nested_function('first', 'nested');
INSERT INTO trigger_context_schema.trigger_context_target VALUES (1);
SELECT name, timing, level, operation, relid = relation.oid,
       table_name, table_schema, nargs, first_argument, second_argument,
       relation_name, context_types
  FROM trigger_context_audit
  CROSS JOIN pg_class relation
 WHERE relation.relname = 'trigger_context_target';
SELECT value FROM trigger_context_conflict;
DROP TABLE trigger_context_schema.trigger_context_target;
DROP SCHEMA trigger_context_schema;
DROP TABLE trigger_context_audit, trigger_context_conflict;
DROP FUNCTION trigger_context_function();
DROP FUNCTION trigger_context_nested_function();
