\pset null 'NULL'

CREATE SCHEMA catalog_ref_a;
CREATE SCHEMA catalog_ref_b;
CREATE COLLATION catalog_ref_a.same (PROVIDER = libc, LOCALE = 'C');
CREATE COLLATION catalog_ref_b.same (PROVIDER = libc, LOCALE = 'C');
CREATE COLLATION catalog_ref_a."dot.name" (PROVIDER = libc, LOCALE = 'C');
CREATE TABLE catalog_ref_a.same (id integer);
CREATE TYPE catalog_ref_a.mood AS ENUM ('calm', 'busy');
CREATE FUNCTION catalog_ref_a.pick(value integer) RETURNS integer
LANGUAGE SQL IMMUTABLE AS $$ SELECT value $$;
CREATE ROLE catalog_ref_role;
SET search_path = catalog_ref_a, catalog_ref_b, pg_catalog;

CREATE TABLE catalog_reference_values (
    value regcollation,
    values regcollation[]
);
INSERT INTO catalog_reference_values
VALUES ('same', ARRAY['same', 'catalog_ref_b.same']::regcollation[]);
CREATE INDEX catalog_reference_values_collation_idx
ON catalog_reference_values (value);
CREATE TABLE catalog_type_reference_values (
    value regtype,
    values regtype[]
);
INSERT INTO catalog_type_reference_values
VALUES ('mood'::regtype, ARRAY['mood']::regtype[]);

SELECT value, values, pg_typeof(value), pg_typeof(values)
FROM catalog_reference_values;
SELECT count(*)
FROM catalog_reference_values
WHERE value = 'same'::regcollation;

SELECT to_regclass('same'),
       to_regclass('missing') IS NULL,
       to_regproc('pick'),
       to_regprocedure('pick(integer)'),
       to_regoper('+') IS NULL,
       to_regoperator('+(integer,integer)'),
       to_regtype('varchar(12)'),
       to_regtype('mood'),
       to_regnamespace('CATALOG_REF_A'),
       to_regrole('CATALOG_REF_ROLE'),
       to_regcollation('SAME'),
       to_regcollation('catalog_ref_b.same'),
       to_regcollation('missing') IS NULL;

SELECT to_regtypemod('varchar(12)'),
       to_regtypemod('numeric(10,3)'),
       to_regtypemod('timestamp(2) with time zone'),
       to_regtypemod('integer'),
       to_regtypemod('missing') IS NULL;

SELECT '0'::regcollation,
       '999999'::regcollation,
       '4294967295'::regcollation,
       'pg_catalog."C"'::regcollation,
       'pg_catalog.default'::regcollation;

SELECT oid, typname, typlen, typtype, typcategory, typelem, typarray,
       typinput, typoutput
FROM pg_type
WHERE oid IN (4191, 4192)
ORDER BY oid;

SELECT (SELECT count(*) FROM pg_operator
        WHERE oprleft IN (4191, 4192) OR oprright IN (4191, 4192)),
       (SELECT count(*) FROM pg_opclass WHERE opcintype IN (4191, 4192));

SELECT oid, proname, prorettype, proargtypes::text, provolatile,
       proparallel, proisstrict
FROM pg_proc
WHERE oid IN (3476, 3479, 3492, 3493, 3494, 3495, 4086, 4093, 4195, 6317)
ORDER BY oid;

SELECT castsource, casttarget, castfunc, castcontext, castmethod
FROM pg_cast
WHERE castsource = 4191 OR casttarget = 4191
ORDER BY oid;

CREATE VIEW catalog_reference_view AS
SELECT 'same'::regcollation AS value;
CREATE VIEW dotted_catalog_reference_view AS
SELECT '"dot.name"'::regcollation AS value;
ALTER COLLATION catalog_ref_a.same RENAME TO renamed;
SELECT value FROM catalog_reference_view;
SELECT to_regcollation('catalog_ref_a.same') IS NULL,
       to_regcollation('catalog_ref_a.renamed');
ALTER COLLATION catalog_ref_a."dot.name" RENAME TO "dot.renamed";
SELECT value FROM dotted_catalog_reference_view;

ALTER TYPE catalog_ref_a.mood RENAME TO mood_v2;
SELECT value, values FROM catalog_type_reference_values;

SELECT to_regcollation('-1') IS NULL;
SELECT '-1'::regcollation;
SELECT to_regtype('-1');
SELECT to_regcollation('database.schema.name');
SELECT to_regtype('database.schema.name');
SELECT to_regproc('database.schema.name');
SELECT to_regprocedure('database.schema.name(integer)');

RESET search_path;
DROP SCHEMA catalog_ref_a CASCADE;
DROP SCHEMA catalog_ref_b CASCADE;
DROP ROLE catalog_ref_role;
