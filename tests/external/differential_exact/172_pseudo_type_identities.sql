-- Pseudo-types identify routine signatures: regprocedure and regtype input,
-- GRANT on a built-in input function, object identities, and the language
-- validators' messages for pseudo-type arguments and results.
SELECT 'int4in(cstring)'::regprocedure, 'array_in(cstring,oid,integer)'::regprocedure::oid;
SELECT 'count("any")'::regprocedure, 'format(text,"any")'::regprocedure;
SELECT 'pg_catalog.int4in(pg_catalog.cstring)'::regprocedure::oid;
SELECT 'cstring'::regtype, '"any"'::regtype, 'pg_catalog."any"'::regtype,
       'trigger'::regtype, 'event_trigger'::regtype, 'language_handler'::regtype,
       'fdw_handler'::regtype, 'tsm_handler'::regtype, 'index_am_handler'::regtype,
       'table_am_handler'::regtype, 'unknown'::regtype, 'internal'::regtype;
SELECT 'cstring[]'::regtype, '_cstring'::regtype, 'record[]'::regtype,
       'pg_catalog.int4'::regtype, 'int4[]'::regtype, 'INTEGER [3]'::regtype,
       '"char"'::regtype, 'character varying(5)'::regtype, 'anyelement'::regtype,
       'pg_class'::regtype, 'timestamp(3) with time zone'::regtype;
SELECT 'any'::regtype;
SELECT '"CSTRING"'::regtype;
SELECT 2275::regtype, 2276::regtype, 1263::regtype, 3115::regtype, 83::regtype;
SELECT pg_identify_object('pg_proc'::regclass, x, 0),
       pg_identify_object_as_address('pg_proc'::regclass, x, 0),
       pg_describe_object('pg_proc'::regclass, x, 0)
FROM unnest(ARRAY['json_in(cstring)'::regprocedure, 'format(text,"any")',
                  'int4pl(int4,int4)', 'count("any")', 'now()',
                  'varchar(varchar,int,bool)', 'max(int4)']) AS x;
CREATE ROLE pseudo_grantee;
GRANT EXECUTE ON FUNCTION int4in(cstring) TO pseudo_grantee;
SELECT has_function_privilege('pseudo_grantee', 'int4in(cstring)', 'EXECUTE');
REVOKE EXECUTE ON FUNCTION int4in(cstring) FROM pseudo_grantee;
GRANT EXECUTE ON FUNCTION format(text, "any") TO pseudo_grantee;
REVOKE EXECUTE ON FUNCTION format(text, "any") FROM pseudo_grantee;
DROP ROLE pseudo_grantee;
CREATE FUNCTION pf(cstring) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf("any") RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf(x any) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf(trigger) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf(unknown) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf(record) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf(internal) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf(void) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf(pg_ddl_command) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf(variadic "any") RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf(cstring) RETURNS int LANGUAGE plpgsql AS 'BEGIN RETURN 1; END';
CREATE FUNCTION pf(internal) RETURNS int LANGUAGE plpgsql AS 'BEGIN RETURN 1; END';
CREATE PROCEDURE pp("any") LANGUAGE plpgsql AS 'BEGIN END';
CREATE PROCEDURE pp(cstring) LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf() RETURNS cstring LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf() RETURNS setof cstring LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf() RETURNS "any" LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf() RETURNS unknown LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf() RETURNS language_handler LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf() RETURNS trigger LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf() RETURNS event_trigger LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf() RETURNS cstring LANGUAGE plpgsql AS 'BEGIN RETURN 1; END';
CREATE FUNCTION pf(int) RETURNS fdw_handler LANGUAGE plpgsql AS 'BEGIN RETURN 1; END';
CREATE FUNCTION pf() RETURNS internal LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf() RETURNS internal LANGUAGE plpgsql AS 'BEGIN RETURN 1; END';
CREATE FUNCTION pf(OUT a cstring) LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf(INOUT a cstring) LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf(OUT a cstring) LANGUAGE plpgsql AS 'BEGIN END';
CREATE FUNCTION pf(x int, OUT y trigger) LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf() RETURNS TABLE (a cstring) LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION pf(anyelement) RETURNS cstring LANGUAGE sql AS 'SELECT 1';
CREATE AGGREGATE pa(cstring) (sfunc = int4pl, stype = int);
CREATE AGGREGATE pa(int) (sfunc = no_such_function, stype = int);
CREATE AGGREGATE pa(int) (sfunc = int4pl, stype = text);
SELECT 'x'::trigger;
SELECT NULL::fdw_handler;
SELECT CAST(NULL AS event_trigger);
SELECT NULL::tsm_handler, 1;
SELECT 'Foo'::regtype;
SELECT 'pg_catalog.Foo'::regtype;
SELECT 'public.Foo[]'::regtype;
SELECT 'foo(3)'::regtype;
SELECT 'serial'::regtype;
SELECT 'nosuch.foo'::regtype;
CREATE TYPE "MyT" AS ENUM ('a');
CREATE DOMAIN public.dd AS int;
SELECT '"MyT"'::regtype, 'public."MyT"[]'::regtype, 'dd'::regtype, 'public.dd'::regtype, 'dd[]'::regtype;
SELECT to_regtype('cstring'), to_regtype('"any"'), to_regtype('nosuch'), to_regtype('_cstring');
SELECT format_type('cstring'::regtype, NULL), format_type(2276, NULL);
SELECT 'double precision'::regtype, 'int8 []'::regtype, 'bit varying(3)'::regtype,
       'time with time zone'::regtype;
PREPARE pt(regtype) AS SELECT $1;
EXECUTE pt('dd');
EXECUTE pt('"MyT"');
DEALLOCATE pt;
DROP DOMAIN dd;
DROP TYPE "MyT";
