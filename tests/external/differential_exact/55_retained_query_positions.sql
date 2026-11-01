CREATE TABLE query_position_source(value integer PRIMARY KEY);
INSERT INTO query_position_source VALUES(1),(2),(3);
CREATE TABLE query_position_audit(value integer);
CREATE FUNCTION query_position_write(value integer) RETURNS integer LANGUAGE sql VOLATILE AS $$INSERT INTO query_position_audit VALUES($1); SELECT $1$$;
SELECT set_config('application_name','0',false);
SELECT set_config('application_name',(current_setting('application_name')::integer+1)::text,false),query_position_write(value) FROM query_position_source;
SELECT current_setting('application_name');
SELECT value FROM query_position_audit ORDER BY value;

TRUNCATE query_position_audit;
SELECT set_config('application_name','0',false);
SELECT query_position_write(20) FROM query_position_source WHERE set_config('application_name',(current_setting('application_name')::integer+1)::text,false)::integer+query_position_write(0)>0;
SELECT current_setting('application_name');
SELECT value,count(*) FROM query_position_audit GROUP BY value ORDER BY value;

TRUNCATE query_position_audit;
SELECT set_config('application_name','0',false);
SELECT set_config('application_name',(current_setting('application_name')::integer+1)::text,false),query_position_write(99) FROM query_position_source WHERE query_position_write(0)=0 LIMIT 1 OFFSET 2;
SELECT current_setting('application_name');
SELECT value,count(*) FROM query_position_audit GROUP BY value ORDER BY value;

TRUNCATE query_position_audit;
SELECT set_config('application_name','0',false);
SELECT set_config('application_name',(current_setting('application_name')::integer+1)::text,false),query_position_write(query_position_write(7)),set_config('application_name',(current_setting('application_name')::integer+1)::text,false),query_position_write(8),set_config('application_name',(current_setting('application_name')::integer+1)::text,false) FROM query_position_source WHERE value=2;
SELECT current_setting('application_name');
SELECT value,count(*) FROM query_position_audit GROUP BY value ORDER BY value;

TRUNCATE query_position_audit;
CREATE SCHEMA query_first;
CREATE SCHEMA query_second;
CREATE FUNCTION query_first.query_target_write(value integer) RETURNS integer LANGUAGE sql VOLATILE AS $$INSERT INTO public.query_position_audit VALUES($1+10); SELECT $1+10$$;
CREATE FUNCTION query_second.query_target_write(value integer) RETURNS integer LANGUAGE sql VOLATILE AS $$INSERT INTO public.query_position_audit VALUES($1+20); SELECT $1+20$$;
SET search_path=query_first,public;
SELECT set_config('search_path','query_second,public',false),query_target_write(value) FROM public.query_position_source WHERE value=2;
SELECT value FROM public.query_position_audit;
SELECT query_target_write(value) FROM public.query_position_source WHERE value=2;
SELECT value FROM public.query_position_audit ORDER BY value;
CREATE FUNCTION query_second.lower(value text) RETURNS text LANGUAGE sql IMMUTABLE AS $$SELECT 'shadow'::text$$;
SET search_path=pg_catalog,public;
SELECT set_config('search_path','query_second,pg_catalog,public',false),lower(value::text),public.query_position_write(value) FROM public.query_position_source WHERE value=2;
SELECT lower(value::text) FROM public.query_position_source WHERE value=2;
RESET search_path;
DROP FUNCTION query_second.lower(text);
DROP FUNCTION query_first.query_target_write(integer);
DROP FUNCTION query_second.query_target_write(integer);
DROP SCHEMA query_first;
DROP SCHEMA query_second;

CREATE FUNCTION query_position_cast_value(value text) RETURNS integer LANGUAGE sql IMMUTABLE AS $$SELECT 777$$;
CREATE FUNCTION query_position_install_cast(value integer) RETURNS integer LANGUAGE sql VOLATILE AS $$CREATE CAST (text AS integer) WITH FUNCTION public.query_position_cast_value(text) AS ASSIGNMENT; SELECT $1$$;
SELECT query_position_install_cast(value),value::text::integer FROM query_position_source WHERE value=2;
SELECT value::text::integer FROM query_position_source WHERE value=2;
DROP CAST (text AS integer);
DROP FUNCTION query_position_install_cast(integer);
DROP FUNCTION query_position_cast_value(text);

TRUNCATE query_position_audit;
CREATE FUNCTION query_position_default(value integer DEFAULT 1) RETURNS integer LANGUAGE sql VOLATILE AS $$INSERT INTO public.query_position_audit VALUES($1); SELECT $1$$;
CREATE FUNCTION query_position_replace_default() RETURNS integer LANGUAGE sql VOLATILE AS $replace$CREATE OR REPLACE FUNCTION public.query_position_default(value integer DEFAULT 9) RETURNS integer LANGUAGE sql VOLATILE AS $value$INSERT INTO public.query_position_audit VALUES($1+100); SELECT $1+100$value$; SELECT 0$replace$;
SELECT query_position_replace_default(),query_position_default() FROM query_position_source WHERE value=2;
SELECT query_position_default() FROM query_position_source WHERE value=2;
SELECT value FROM query_position_audit ORDER BY value;
DROP FUNCTION query_position_replace_default();
DROP FUNCTION query_position_default(integer);
DROP FUNCTION query_position_write(integer);
DROP TABLE query_position_audit;
DROP TABLE query_position_source;
RESET application_name;
