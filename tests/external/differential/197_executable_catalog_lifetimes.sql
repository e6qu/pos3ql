CREATE SCHEMA lifetime_exec_moved;

CREATE TYPE lifetime_exec_value AS ENUM ('low', 'high');

CREATE FUNCTION lifetime_exec_same(integer, integer) RETURNS boolean LANGUAGE SQL IMMUTABLE RETURN $1 = $2;

CREATE FUNCTION lifetime_exec_compare(integer, integer) RETURNS integer LANGUAGE SQL IMMUTABLE RETURN CASE WHEN $1 < $2 THEN -1 WHEN $1 > $2 THEN 1 ELSE 0 END;

CREATE FUNCTION lifetime_exec_text(lifetime_exec_value, integer) RETURNS text LANGUAGE SQL IMMUTABLE RETURN CASE WHEN $1 = 'low' THEN 'low-cast' ELSE 'high-cast' END;

CREATE FUNCTION lifetime_exec_event() RETURNS event_trigger LANGUAGE plpgsql AS 'BEGIN RETURN; END';

-- executable objects
CREATE ACCESS METHOD lifetime_exec_heap TYPE TABLE HANDLER heap_tableam_handler;

CREATE CAST (lifetime_exec_value AS text) WITH FUNCTION lifetime_exec_text AS ASSIGNMENT;

CREATE OPERATOR === (FUNCTION = lifetime_exec_same, LEFTARG = integer, RIGHTARG = integer);

CREATE OPERATOR FAMILY lifetime_exec_family USING btree;

CREATE OPERATOR CLASS lifetime_exec_class FOR TYPE integer USING btree FAMILY lifetime_exec_family AS OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 ===, OPERATOR 4 >=, OPERATOR 5 >, FUNCTION 1 lifetime_exec_compare(integer, integer);

CREATE EVENT TRIGGER lifetime_exec_trigger ON ddl_command_end EXECUTE FUNCTION lifetime_exec_event();

COMMENT ON ACCESS METHOD lifetime_exec_heap IS 'original access method';

COMMENT ON CAST (lifetime_exec_value AS text) IS 'original cast';

COMMENT ON OPERATOR === (integer, integer) IS 'original operator';

COMMENT ON OPERATOR FAMILY lifetime_exec_family USING btree IS 'original family';

COMMENT ON OPERATOR CLASS lifetime_exec_class USING btree IS 'original class';

COMMENT ON EVENT TRIGGER lifetime_exec_trigger IS 'original event trigger';

CREATE TABLE lifetime_exec_rows(value integer) USING lifetime_exec_heap;

CREATE INDEX lifetime_exec_index ON lifetime_exec_rows(value lifetime_exec_class);

INSERT INTO lifetime_exec_rows VALUES (7), (2);

CREATE VIEW lifetime_exec_report AS SELECT value, value OPERATOR(public.===) value AS same, 'low'::lifetime_exec_value::text AS label FROM lifetime_exec_rows;

CREATE VIEW lifetime_exec_catalog AS
SELECT 'access method' AS kind, oid AS identity FROM pg_am WHERE amname = 'lifetime_exec_heap'
UNION ALL SELECT 'cast', oid FROM pg_cast WHERE castsource = 'lifetime_exec_value'::regtype AND casttarget = 'text'::regtype
UNION ALL SELECT 'operator', oid FROM pg_operator WHERE oprname = '==='
UNION ALL SELECT 'family', oid FROM pg_opfamily WHERE opfname IN ('lifetime_exec_family', 'lifetime_exec_family_moved')
UNION ALL SELECT 'class', oid FROM pg_opclass WHERE opcname IN ('lifetime_exec_class', 'lifetime_exec_class_moved')
UNION ALL SELECT 'event trigger', oid FROM pg_event_trigger WHERE evtname IN ('lifetime_exec_trigger', 'lifetime_exec_trigger_moved');

CREATE TABLE saved_exec_lifetimes AS SELECT * FROM lifetime_exec_catalog;

SELECT count(*) = 6 FROM lifetime_exec_catalog;

SELECT 'low'::lifetime_exec_value::text = 'low-cast';

SELECT value OPERATOR(public.===) 7 FROM lifetime_exec_rows WHERE value = 7;

SELECT count(*) = 2 FROM lifetime_exec_report WHERE same AND label = 'low-cast';

SELECT obj_description(oid, 'pg_am') = 'original access method' FROM pg_am WHERE amname = 'lifetime_exec_heap';

SELECT obj_description(oid, 'pg_cast') = 'original cast' FROM pg_cast WHERE castsource = 'lifetime_exec_value'::regtype AND casttarget = 'text'::regtype;

-- mutation checks
BEGIN;

SAVEPOINT before_exec_rename;

ALTER OPERATOR FAMILY lifetime_exec_family USING btree RENAME TO lifetime_exec_family_moved;

ALTER OPERATOR CLASS lifetime_exec_class USING btree RENAME TO lifetime_exec_class_moved;

ALTER EVENT TRIGGER lifetime_exec_trigger RENAME TO lifetime_exec_trigger_moved;

ALTER OPERATOR public.=== (integer, integer) SET SCHEMA lifetime_exec_moved;

SELECT count(*) = 6 FROM saved_exec_lifetimes s JOIN lifetime_exec_catalog c ON s.kind = c.kind AND s.identity = c.identity;

SELECT count(*) = 2 FROM lifetime_exec_report WHERE same AND label = 'low-cast';

ROLLBACK TO before_exec_rename;

COMMIT;

DROP VIEW lifetime_exec_report;

DROP TABLE lifetime_exec_rows;

DROP OPERATOR CLASS lifetime_exec_class USING btree;

DROP OPERATOR FAMILY lifetime_exec_family USING btree;

DROP OPERATOR public.=== (integer, integer);

DROP CAST (lifetime_exec_value AS text);

DROP ACCESS METHOD lifetime_exec_heap;

DROP EVENT TRIGGER lifetime_exec_trigger;

CREATE ACCESS METHOD lifetime_exec_heap TYPE TABLE HANDLER heap_tableam_handler;

CREATE CAST (lifetime_exec_value AS text) WITH FUNCTION lifetime_exec_text AS ASSIGNMENT;

CREATE OPERATOR === (FUNCTION = lifetime_exec_same, LEFTARG = integer, RIGHTARG = integer);

CREATE OPERATOR FAMILY lifetime_exec_family USING btree;

CREATE OPERATOR CLASS lifetime_exec_class FOR TYPE integer USING btree FAMILY lifetime_exec_family AS OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 ===, OPERATOR 4 >=, OPERATOR 5 >, FUNCTION 1 lifetime_exec_compare(integer, integer);

CREATE EVENT TRIGGER lifetime_exec_trigger ON ddl_command_end EXECUTE FUNCTION lifetime_exec_event();

CREATE TABLE lifetime_exec_rows(value integer) USING lifetime_exec_heap;

CREATE INDEX lifetime_exec_index ON lifetime_exec_rows(value lifetime_exec_class);

INSERT INTO lifetime_exec_rows VALUES (7), (2);

CREATE VIEW lifetime_exec_report AS SELECT value, value OPERATOR(public.===) value AS same, 'low'::lifetime_exec_value::text AS label FROM lifetime_exec_rows;

SELECT count(*) = 0 FROM saved_exec_lifetimes s JOIN lifetime_exec_catalog c ON s.kind = c.kind AND s.identity = c.identity;

SELECT obj_description(oid, 'pg_am') IS NULL FROM pg_am WHERE amname = 'lifetime_exec_heap';

SELECT obj_description(oid, 'pg_cast') IS NULL FROM pg_cast WHERE castsource = 'lifetime_exec_value'::regtype AND casttarget = 'text'::regtype;

DROP VIEW lifetime_exec_catalog;

DROP TABLE saved_exec_lifetimes;

DROP VIEW lifetime_exec_report;

DROP TABLE lifetime_exec_rows;

DROP OPERATOR CLASS lifetime_exec_class USING btree;

DROP OPERATOR FAMILY lifetime_exec_family USING btree;

DROP OPERATOR public.=== (integer, integer);

DROP CAST (lifetime_exec_value AS text);

DROP ACCESS METHOD lifetime_exec_heap;

DROP EVENT TRIGGER lifetime_exec_trigger;

DROP FUNCTION lifetime_exec_event();

DROP FUNCTION lifetime_exec_same(integer, integer);

DROP FUNCTION lifetime_exec_compare(integer, integer);

DROP FUNCTION lifetime_exec_text(lifetime_exec_value, integer);

DROP TYPE lifetime_exec_value;

DROP SCHEMA lifetime_exec_moved;
