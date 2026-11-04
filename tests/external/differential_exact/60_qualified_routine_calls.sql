CREATE SEQUENCE qualified_call_values;
CREATE FUNCTION public.nextval(regclass) RETURNS bigint LANGUAGE sql AS $$SELECT 999::bigint$$;
CREATE FUNCTION public.abs(integer) RETURNS integer LANGUAGE sql AS $$SELECT 999$$;
CREATE FUNCTION public.version() RETURNS text LANGUAGE sql AS $$SELECT 'shadow'::text$$;
CREATE FUNCTION public.qualified_call_default(value bigint DEFAULT pg_catalog.nextval('qualified_call_values')) RETURNS bigint LANGUAGE sql AS $$SELECT $1$$;
CREATE AGGREGATE public.sum(integer) (SFUNC=pg_catalog.int4pl, STYPE=integer, INITCOND='100');
SET search_path TO public,pg_catalog;
SELECT nextval('qualified_call_values'),abs(-3),version();
SELECT pg_catalog.nextval('qualified_call_values'),"pg_catalog".nextval('qualified_call_values'),
       pg_catalog.abs(-3),"pg_catalog".abs(-3),pg_catalog.version()<>'shadow',"pg_catalog".version()<>'shadow';
SELECT qualified_call_default();
SELECT sum(value),pg_catalog.sum(value),"pg_catalog".sum(value),pg_catalog.count(*),"pg_catalog".count(*)
FROM (VALUES(1),(2)) AS input(value);
SELECT value,pg_catalog.row_number() OVER(ORDER BY value),"pg_catalog".row_number() OVER(ORDER BY value)
FROM (VALUES(2),(1)) AS input(value) ORDER BY value;
SELECT position('a' IN 'ba'),pg_catalog.position('a','ba'),"pg_catalog".position('a','ba'),
       pg_catalog.make_interval(years=>1)=INTERVAL '1 year',"pg_catalog".make_interval(years=>1)=INTERVAL '1 year';
SELECT pg_catalog.current_user()=current_user,pg_catalog.session_user()=session_user,
       pg_catalog.current_schema()=current_schema,pg_catalog.normalize('x','NFC');
SELECT pg_catalog.int4 '7',"pg_catalog".int4 '8',pg_catalog.numeric(6,2) '1.25';
SELECT pg_catalog.pg_class.relname FROM pg_catalog.pg_class WHERE pg_catalog.pg_class.relname='qualified_call_values';
CREATE SCHEMA qualified_column_scope;
CREATE TABLE qualified_column_scope.values_table(value integer);
INSERT INTO qualified_column_scope.values_table VALUES(17);
SELECT qualified_column_scope.values_table.value FROM qualified_column_scope.values_table;
RESET search_path;
DROP TABLE qualified_column_scope.values_table;
DROP SCHEMA qualified_column_scope;
DROP FUNCTION public.qualified_call_default(bigint);
DROP AGGREGATE public.sum(integer);
DROP FUNCTION public.nextval(regclass);
DROP FUNCTION public.abs(integer);
DROP FUNCTION public.version();
DROP SEQUENCE qualified_call_values;
CREATE SEQUENCE qualified_error_values;

-- next request
SELECT pg_catalog.coalesce(1,2);
-- next request
SELECT "pg_catalog".coalesce(1,2);
-- next request
SELECT pg_catalog.nullif(1,2);
-- next request
SELECT pg_catalog.greatest(1,2);
-- next request
SELECT pg_catalog.least(1,2);
-- next request
SELECT pg_catalog.trim(' x ');
-- next request
SELECT pg_catalog.row(1);
-- next request
SELECT pg_catalog.current_date();
-- next request
SELECT pg_catalog.current_timestamp();
-- next request
SELECT pg_catalog.current_role();
-- next request
SELECT pg_catalog.current_catalog();
-- next request
SELECT pg_catalog.localtime();
-- next request
SELECT pg_catalog.localtimestamp();
-- next request
SELECT pg_catalog.user();
-- next request
SELECT pg_catalog.array(1);
-- next request
SELECT pg_catalog.json_array(1);
-- next request
SELECT information_schema.abs(-1);
-- next request
SELECT pg_catalog.no_such_func(1);
-- next request
SELECT pg_catalog.json_object('x':1);

-- next request
SELECT pg_catalog.array[1];
-- next request
SELECT pg_catalog.current_date;
-- next request
SELECT pg_catalog.pg_class.relname FROM pg_catalog.pg_class AS hidden;
-- next request
SELECT pg_catalog.coalesce(pg_catalog.nextval('qualified_error_values'),1);
-- next request
SELECT pg_catalog.nextval('qualified_error_values');
DROP SEQUENCE qualified_error_values;
