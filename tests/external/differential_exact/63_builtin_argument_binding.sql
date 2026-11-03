CREATE FUNCTION public.make_interval(value integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT $1+100';
CREATE SEQUENCE builtin_binding_effects;
SET search_path=public,pg_catalog;
SELECT make_interval(7),pg_typeof(make_interval(7));
SET search_path=pg_catalog,public;
SELECT make_interval(1),pg_typeof(make_interval(1));
SELECT pg_catalog.make_interval();
SELECT pg_catalog.make_interval(days=>2);
SELECT array_to_string(pg_catalog.parse_ident(str=>'"Mixed".bare'),',');
ALTER SEQUENCE builtin_binding_effects RESTART WITH 1;
SELECT pg_catalog.make_interval(secs=>nextval('builtin_binding_effects'),years=>nextval('builtin_binding_effects')::integer);
SELECT nextval('builtin_binding_effects');
ALTER SEQUENCE builtin_binding_effects RESTART WITH 1;
SELECT (pg_catalog.regexp_matches(
  flags=>CASE WHEN nextval('builtin_binding_effects')=3 THEN 'g' ELSE 'i' END,
  pattern=>CASE WHEN nextval('builtin_binding_effects')=2 THEN 'a' ELSE 'z' END,
  string=>CASE WHEN nextval('builtin_binding_effects')=1 THEN 'aba' ELSE 'none' END))[1];
SELECT nextval('builtin_binding_effects');
ALTER SEQUENCE builtin_binding_effects RESTART WITH 1;
SELECT pg_catalog.make_interval(years=>NULL,days=>nextval('builtin_binding_effects')::integer) IS NULL;
SELECT nextval('builtin_binding_effects');
ALTER SEQUENCE builtin_binding_effects RESTART WITH 1;
SELECT pg_catalog.make_interval(days=>NULL,years=>nextval('builtin_binding_effects')::integer) IS NULL;
SELECT nextval('builtin_binding_effects');
ALTER SEQUENCE builtin_binding_effects RESTART WITH 1;
-- next request
SELECT pg_catalog.make_interval(unknown=>nextval('builtin_binding_effects'));
-- next request
SELECT nextval('builtin_binding_effects');
SELECT pg_catalog.pg_stat_reset_shared(NULL);
SELECT pg_catalog.pg_stat_reset_shared();
SELECT pg_catalog.pg_stat_reset_slru();
SELECT
  json_strip_nulls('{"drop":null,"nested":[null,{"gone":null,"keep":[null,1]},null,[null,2]]}'::json)::jsonb = '{"nested":[null,{"keep":[null,1]},null,[null,2]]}'::jsonb,
  json_strip_nulls('{"drop":null,"nested":[null,{"gone":null,"keep":[null,1]},null,[null,2]]}'::json,false)::jsonb = '{"nested":[null,{"keep":[null,1]},null,[null,2]]}'::jsonb,
  json_strip_nulls('{"drop":null,"nested":[null,{"gone":null,"keep":[null,1]},null,[null,2]]}'::json,true)::jsonb = '{"nested":[{"keep":[1]},[2]]}'::jsonb,
  jsonb_strip_nulls('{"drop":null,"nested":[null,{"gone":null,"keep":[null,1]},null,[null,2]]}'::jsonb) = '{"nested":[null,{"keep":[null,1]},null,[null,2]]}'::jsonb,
  jsonb_strip_nulls('{"drop":null,"nested":[null,{"gone":null,"keep":[null,1]},null,[null,2]]}'::jsonb,false) = '{"nested":[null,{"keep":[null,1]},null,[null,2]]}'::jsonb,
  jsonb_strip_nulls('{"drop":null,"nested":[null,{"gone":null,"keep":[null,1]},null,[null,2]]}'::jsonb,true) = '{"nested":[{"keep":[1]},[2]]}'::jsonb;
SELECT json_strip_nulls('null'::json,true)::text,
       jsonb_strip_nulls('null'::jsonb,true)::text,
       json_strip_nulls(NULL::json,true) IS NULL,
       jsonb_strip_nulls(NULL::jsonb,true) IS NULL,
       json_strip_nulls('[null]'::json,NULL) IS NULL,
       jsonb_strip_nulls('[null]'::jsonb,NULL) IS NULL,
       json_strip_nulls('[null,[null],{"a":null,"b":[null]}]'::json,'true')::jsonb = '[[],{"b":[]}]'::jsonb;
DROP FUNCTION public.make_interval(integer);
DROP SEQUENCE builtin_binding_effects;
RESET search_path;
