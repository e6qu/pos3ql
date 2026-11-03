CREATE TABLE qualified_set_keys(a integer,b integer,PRIMARY KEY(a,b));
CREATE SEQUENCE qualified_set_effects;
SELECT (expanded.value).x,(expanded.value).n
FROM (SELECT information_schema._pg_expandarray('[4:6]={10,NULL,30}'::integer[]) AS value) expanded
ORDER BY (expanded.value).n;
SELECT (information_schema._pg_expandarray(ARRAY['x',NULL,'z'])).*;
SELECT count(*) FROM (SELECT information_schema._pg_expandarray(ARRAY[]::integer[])) empty_set;
SELECT count(*) FROM (SELECT information_schema._pg_expandarray(NULL::integer[])) null_set;
SELECT (expanded.value).x,(expanded.value).n
FROM (SELECT information_schema._pg_expandarray('[2:3][5:6]={{1,2},{3,4}}'::integer[]) AS value) expanded
ORDER BY (expanded.value).n;
SELECT * FROM information_schema._pg_expandarray('[4:5]={40,50}'::integer[]);
SELECT pg_catalog.generate_series(2,3);
SELECT "pg_catalog".unnest(ARRAY[4,5]);
SELECT (pg_catalog.pg_options_to_table(ARRAY['host=localhost','port=5432'])).*;
SELECT (pg_catalog.regexp_matches(flags=>'g',pattern=>'a',string=>'abac'))[1];
SELECT (information_schema._pg_expandarray(i.indkey)).n AS seq,
       (information_schema._pg_expandarray(i.indkey)).x AS att
FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid=i.indexrelid
WHERE c.relname='qualified_set_keys_pkey';

-- next request
SELECT _pg_expandarray(ARRAY[pg_catalog.nextval('qualified_set_effects')]);
-- next request
SELECT pg_catalog._pg_expandarray(ARRAY[pg_catalog.nextval('qualified_set_effects')]);
-- next request
SELECT information_schema.unnest(ARRAY[pg_catalog.nextval('qualified_set_effects')]);
-- next request
SELECT pg_catalog.nextval('qualified_set_effects');
SET search_path TO information_schema,public;
SELECT (_pg_expandarray(ARRAY[8,9])).x,(_pg_expandarray(ARRAY[8,9])).n;
RESET search_path;
CREATE FUNCTION public."__json_arrayagg_absent_json"(integer) RETURNS integer
LANGUAGE sql IMMUTABLE AS $$SELECT $1+100$$;
CREATE FUNCTION public."coalesce"(integer,integer) RETURNS integer
LANGUAGE sql IMMUTABLE AS $$SELECT $1+$2+100$$;
CREATE FUNCTION public."row"(integer) RETURNS integer
LANGUAGE sql IMMUTABLE AS $$SELECT $1+100$$;
SELECT "__json_arrayagg_absent_json"(2),COALESCE(NULL,3),"coalesce"(1,2),"row"(4),ROW(5)::text;
SELECT * FROM unnest(ARRAY[1,2],ARRAY[3,4]);
CREATE FUNCTION public.unnest(integer[]) RETURNS SETOF integer
LANGUAGE sql IMMUTABLE AS $$SELECT 99$$;
SET search_path TO pg_catalog,public;
SELECT unnest(ARRAY[1,2]);
SELECT pg_catalog.unnest(ARRAY[1,2]);
RESET search_path;
DROP FUNCTION public.unnest(integer[]);
DROP FUNCTION public."__json_arrayagg_absent_json"(integer);
DROP FUNCTION public."coalesce"(integer,integer);
DROP FUNCTION public."row"(integer);
DROP TABLE qualified_set_keys;
DROP SEQUENCE qualified_set_effects;
-- next request
SELECT COALESCE(DISTINCT 1,2);
-- next request
SELECT ROW(VARIADIC ARRAY[1]);
-- next request
SELECT NULLIF(1,2) FILTER (WHERE true);
-- next request
SELECT GREATEST(a=>1,b=>2);
