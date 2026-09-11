\pset null 'NULL'

DROP SCHEMA IF EXISTS object_introspection_a CASCADE;
DROP SCHEMA IF EXISTS object_introspection_b CASCADE;

CREATE SCHEMA object_introspection_a;
CREATE SCHEMA object_introspection_b;
CREATE TABLE object_introspection_a.items (
    id serial PRIMARY KEY,
    value text NOT NULL,
    amount integer,
    CHECK (amount >= 0)
);
CREATE TABLE object_introspection_b.items (id integer, value integer);
CREATE INDEX items_value_idx ON object_introspection_a.items (value);
CREATE TYPE object_introspection_a.mood AS ENUM ('calm', 'busy');
CREATE TYPE object_introspection_b.mood AS ENUM ('hidden');
CREATE DOMAIN object_introspection_a.positive AS integer CHECK (VALUE > 0);
CREATE FUNCTION object_introspection_a.bump(integer) RETURNS integer
LANGUAGE SQL IMMUTABLE AS $$ SELECT $1 + 1 $$;
CREATE FUNCTION object_introspection_b.bump(integer) RETURNS integer
LANGUAGE SQL IMMUTABLE AS $$ SELECT $1 + 2 $$;
CREATE STATISTICS object_introspection_a.item_stats ON value, amount
FROM object_introspection_a.items;
CREATE STATISTICS object_introspection_b.item_stats ON id, value
FROM object_introspection_b.items;

SELECT (pg_identify_object('pg_class'::regclass, c.oid, 0)).*
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'object_introspection_a' AND c.relname = 'items';

SELECT * FROM pg_identify_object(
    'pg_class'::regclass, 'object_introspection_a.items'::regclass, 0);

SELECT (pg_identify_object('pg_class'::regclass, c.oid, 2)).*
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'object_introspection_a' AND c.relname = 'items';

SELECT (pg_identify_object_as_address('pg_class'::regclass, c.oid, 2)).*
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'object_introspection_a' AND c.relname = 'items';

SELECT pg_describe_object('pg_class'::regclass, c.oid, 2)
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'object_introspection_a' AND c.relname = 'items';

SELECT (pg_identify_object('pg_proc'::regclass, p.oid, 0)).*
FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
WHERE n.nspname = 'object_introspection_a' AND p.proname = 'bump';

SELECT (pg_identify_object_as_address('pg_proc'::regclass, p.oid, 0)).*
FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
WHERE n.nspname = 'object_introspection_a' AND p.proname = 'bump';

SELECT (pg_get_object_address('table column',
           ARRAY['object_introspection_a','items','value'], ARRAY[]::text[])).classid
           = 'pg_class'::regclass,
       (pg_get_object_address('table column',
           ARRAY['object_introspection_a','items','value'], ARRAY[]::text[])).objid = c.oid,
       (pg_get_object_address('table column',
           ARRAY['object_introspection_a','items','value'], ARRAY[]::text[])).objsubid = 2
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'object_introspection_a' AND c.relname = 'items';

SELECT (pg_get_object_address('function',
           ARRAY['object_introspection_a','bump'], ARRAY['integer'])).classid
           = 'pg_proc'::regclass,
       (pg_get_object_address('function',
           ARRAY['object_introspection_a','bump'], ARRAY['integer'])).objid = p.oid
FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
WHERE n.nspname = 'object_introspection_a' AND p.proname = 'bump';

SELECT pg_get_serial_sequence('object_introspection_a.items', 'id'),
       pg_get_serial_sequence('object_introspection_a.items', 'amount');
BEGIN;
ALTER TABLE object_introspection_a.items RENAME TO products;
ALTER TABLE object_introspection_a.products RENAME COLUMN id TO product_id;
SELECT pg_get_serial_sequence('object_introspection_a.products', 'product_id');
ROLLBACK;
SELECT pg_get_serial_sequence('object_introspection_a.items', 'id');

SET search_path = object_introspection_a, object_introspection_b, pg_catalog;
SELECT n.nspname, pg_table_is_visible(c.oid)
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relname = 'items' ORDER BY n.nspname;
SELECT n.nspname, pg_type_is_visible(t.oid)
FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
WHERE t.typname = 'mood' ORDER BY n.nspname;
SELECT n.nspname, pg_function_is_visible(p.oid)
FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
WHERE p.proname = 'bump' ORDER BY n.nspname;
SELECT n.nspname, pg_statistics_obj_is_visible(s.oid)
FROM pg_statistic_ext s JOIN pg_namespace n ON n.oid = s.stxnamespace
WHERE s.stxname = 'item_stats' ORDER BY n.nspname;
SELECT pg_table_is_visible(999999) IS NULL,
       pg_type_is_visible(999999) IS NULL,
       pg_function_is_visible(999999) IS NULL,
       pg_operator_is_visible(999999) IS NULL,
       pg_opclass_is_visible(999999) IS NULL,
       pg_opfamily_is_visible(999999) IS NULL,
       pg_conversion_is_visible(999999) IS NULL,
       pg_statistics_obj_is_visible(999999) IS NULL,
       pg_ts_dict_is_visible(999999) IS NULL,
       pg_ts_config_is_visible(999999) IS NULL;

-- Drivers may select int2 for small Python integer parameters; PostgreSQL
-- widens them at the catalog identity boundary without changing the result.
SELECT (pg_identify_object(
           1255::smallint, 1665::smallint, 0::smallint)).type;

SELECT oid, proname, prorettype, proargtypes::text, proallargtypes,
       proargmodes, proargnames, provolatile, proparallel, proisstrict
FROM pg_proc
WHERE oid IN (1665,2082,2083,2093,3382,3403,3537,3757,3758,3829,3839,3954)
ORDER BY oid;

RESET search_path;
DROP SCHEMA object_introspection_a CASCADE;
DROP SCHEMA object_introspection_b CASCADE;
