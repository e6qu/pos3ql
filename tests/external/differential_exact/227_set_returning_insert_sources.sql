-- Set-returning calls in an INSERT ... SELECT output list expand rows.
CREATE SCHEMA set_returning_insert_sources;
SET search_path = set_returning_insert_sources, public;
CREATE FUNCTION srf_values(start_value integer) RETURNS SETOF integer
LANGUAGE SQL AS $$ SELECT start_value UNION ALL SELECT start_value + 1 $$;
CREATE TABLE target(id integer, label text);
INSERT INTO target SELECT srf_values(20), 'set';
INSERT INTO target SELECT srf_values(30);
INSERT INTO target(id) SELECT srf_values(40) + 1;
INSERT INTO target SELECT g, 'g' FROM generate_series(1, 2) g, srf_values(50) r;
SELECT * FROM target ORDER BY 1, 2;
SELECT pg_options_to_table(ARRAY[NULL, 'empty=']);
SELECT * FROM pg_options_to_table(ARRAY[NULL, 'empty=']);
RESET search_path;
DROP SCHEMA set_returning_insert_sources CASCADE;
