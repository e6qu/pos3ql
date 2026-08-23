-- PostgreSQL table-function composition. Multiple UNNEST arguments and
-- ROWS FROM advance in lockstep, padding shorter results with NULL; ordinality
-- belongs to the combined row source.
SELECT * FROM unnest(ARRAY[1,2,3], ARRAY['a','b']);
SELECT left_value, right_value
  FROM unnest(ARRAY[1,2], ARRAY['a','b','c']) AS u(left_value, right_value)
 ORDER BY COALESCE(left_value, 99);
SELECT * FROM unnest('[0:1]={7,8}'::integer[], ARRAY[true]);
SELECT * FROM unnest(NULL::integer[], ARRAY['only']);
SELECT * FROM unnest(ARRAY[]::integer[], ARRAY[]::text[]);

-- Independent SRFs remain lockstepped even when a scalar expression wraps
-- more than one call; the longest call controls cardinality.
SELECT generate_series(1,2) + generate_series(10,12);
SELECT ROW(generate_series(1,3), unnest(ARRAY['a','b']));

SELECT *
  FROM ROWS FROM (generate_series(1,3), unnest(ARRAY['x','y']));
SELECT series_value, array_value, ordinality
  FROM ROWS FROM (
         generate_series(10,30,10),
         unnest(ARRAY['a','b'])
       ) WITH ORDINALITY AS rf(series_value, array_value, ordinality)
 ORDER BY ordinality;
SELECT key, value, series_value
  FROM ROWS FROM (
         json_each('{"a":1,"b":2}'::json),
         generate_series(7,7)
       ) AS rf(key, value, series_value)
 ORDER BY key;

CREATE TABLE rows_from_input(id integer, values integer[]);
INSERT INTO rows_from_input VALUES (1, ARRAY[2,3]), (2, ARRAY[4]);
SELECT input.id, expanded.value, expanded.ordinality
  FROM rows_from_input AS input,
       LATERAL ROWS FROM (unnest(input.values))
         WITH ORDINALITY AS expanded(value, ordinality)
 ORDER BY input.id, expanded.ordinality;
SELECT input.id, expanded.value
  FROM rows_from_input AS input
  JOIN LATERAL unnest(input.values) AS expanded(value)
    ON expanded.value > input.id
 ORDER BY input.id, expanded.value;

CREATE FUNCTION rows_from_pair(start_value integer)
RETURNS TABLE(item integer, label text)
LANGUAGE SQL
AS $$ SELECT start_value, 'first' UNION ALL SELECT start_value + 1, 'second' $$;

CREATE FUNCTION rows_from_values(start_value integer)
RETURNS SETOF integer
LANGUAGE SQL
AS $$ SELECT start_value UNION ALL SELECT start_value + 1 $$;

SELECT rows_from_values(4), generate_series(10,12);
SELECT rows_from_pair(4), generate_series(10,12);
SELECT (rows_from_pair(4)).*;
SELECT rows_from_values(1) + rows_from_values(10);
SELECT rows_from_values(rows_from_values(1));
SELECT input.id, rows_from_values(input.id)
  FROM rows_from_input AS input
 ORDER BY input.id, 2;
SELECT rows_from_values(max(id)) FROM rows_from_input;
SELECT ARRAY(SELECT rows_from_values(8));
WITH RECURSIVE numbers(value) AS (
  SELECT 1
  UNION ALL
  SELECT value + 1 FROM numbers WHERE value < 2
)
SELECT rows_from_values(value) FROM numbers ORDER BY 1;
WITH target_set AS (
  SELECT rows_from_values(6) AS value
)
SELECT value FROM target_set
UNION ALL
SELECT 9
ORDER BY 1;
CREATE VIEW rows_from_target_view AS
SELECT rows_from_values(7) AS value, generate_series(30,32) AS generated;
SELECT * FROM rows_from_target_view ORDER BY generated;
CREATE MATERIALIZED VIEW rows_from_target_mat AS
SELECT rows_from_values(11) AS value;
SELECT * FROM rows_from_target_mat ORDER BY value;

SELECT item, label, generated
  FROM ROWS FROM (
         rows_from_pair(4),
         generate_series(9,9)
       ) AS rf(item, label, generated)
 ORDER BY item;

CREATE VIEW rows_from_view AS
SELECT item, label, generated, ordinality
  FROM ROWS FROM (
         rows_from_pair(5),
         generate_series(20,21)
       ) WITH ORDINALITY AS rf(item, label, generated, ordinality);
SELECT * FROM rows_from_view ORDER BY ordinality;

WITH expanded AS (
  SELECT * FROM unnest(ARRAY[1,2], ARRAY['p']) AS u(id, label)
)
SELECT id, label FROM expanded
UNION ALL
SELECT 3, 'tail'
ORDER BY 1;

CREATE TABLE rows_from_target(id integer, label text);
INSERT INTO rows_from_target
SELECT * FROM unnest(ARRAY[10,11], ARRAY['ten','eleven']);
INSERT INTO rows_from_target
SELECT rows_from_values(20), 'set';
UPDATE rows_from_target AS target
   SET label = source.label
  FROM unnest(ARRAY[10], ARRAY['TEN']) AS source(id, label)
 WHERE target.id = source.id;
DELETE FROM rows_from_target AS target
 USING unnest(ARRAY[11], ARRAY['eleven']) AS source(id, label)
 WHERE target.id = source.id AND target.label = source.label
 RETURNING target.id;
MERGE INTO rows_from_target AS target
USING unnest(ARRAY[10,30], ARRAY['merged','thirty']) AS source(id, label)
   ON target.id = source.id
 WHEN MATCHED THEN UPDATE SET label = source.label
 WHEN NOT MATCHED THEN INSERT (id, label) VALUES (source.id, source.label);
SELECT * FROM rows_from_target ORDER BY id;

CREATE TABLE rows_from_effects(value integer);
CREATE FUNCTION rows_from_insert(start_value integer)
RETURNS SETOF integer
LANGUAGE SQL
AS $$ INSERT INTO rows_from_effects VALUES (start_value), (start_value + 1) RETURNING value $$;
SELECT rows_from_insert(40), generate_series(1,3);
SELECT * FROM rows_from_effects ORDER BY value;

CREATE FUNCTION rows_from_empty()
RETURNS SETOF integer
LANGUAGE SQL
AS $$ SELECT 1 WHERE false $$;
SELECT rows_from_empty(), generate_series(1,2);

CREATE SCHEMA qualified_srf;
CREATE FUNCTION qualified_srf.text_values(start_value text)
RETURNS SETOF text
LANGUAGE SQL
AS $$ SELECT start_value $$;
CREATE VIEW qualified_srf_view AS
SELECT qualified_srf.text_values('kept') AS value;
BEGIN;
DROP FUNCTION qualified_srf.text_values(text);
ROLLBACK;
ALTER FUNCTION qualified_srf.text_values(text) RENAME TO moved_text_values;
SELECT * FROM qualified_srf_view;
DROP VIEW qualified_srf_view;
DROP FUNCTION qualified_srf.moved_text_values(text);
DROP SCHEMA qualified_srf;

CREATE FUNCTION rows_from_values(start_value text)
RETURNS SETOF text
LANGUAGE SQL
AS $$ SELECT start_value $$;
DROP FUNCTION rows_from_values(text);
ALTER FUNCTION rows_from_values(integer) RENAME TO rows_from_values_moved;
CREATE SCHEMA rows_from_schema;
ALTER FUNCTION rows_from_values_moved(integer) SET SCHEMA rows_from_schema;
SELECT * FROM rows_from_target_view ORDER BY generated;

CREATE FUNCTION rows_from_cascade(start_value integer)
RETURNS SETOF integer
LANGUAGE SQL
AS $$ SELECT start_value $$;
CREATE VIEW rows_from_cascade_view AS SELECT rows_from_cascade(1) AS value;
DROP FUNCTION rows_from_cascade(integer) CASCADE;
SELECT * FROM rows_from_cascade_view;

BEGIN;
DROP FUNCTION rows_from_schema.rows_from_values_moved(integer);
ROLLBACK;

-- Catalog SRFs follow the same project-set and record-expansion rules as
-- ordinary record-returning functions.
SELECT pg_options_to_table(ARRAY['fillfactor=80','flag']);
SELECT (pg_options_to_table(ARRAY['fillfactor=80','flag'])).*;
SELECT (pg_options_to_table(ARRAY[NULL,'empty='])).*;
SELECT unnest();
SELECT unnest(ARRAY[1], ARRAY[2]);
SELECT json_each();
SELECT json_array_elements();
SELECT regexp_split_to_table('a');
SELECT string_to_table('a');
SELECT generate_subscripts(ARRAY[1], 1, false, false);
SELECT pg_options_to_table();
SELECT pg_get_sequence_data();
SELECT unnest(1);
SELECT regexp_matches(1, 'x');
SELECT regexp_matches('x', 'x', 1);
SELECT regexp_split_to_table(1, 'x');
SELECT regexp_split_to_table('x', 'x', 1);
SELECT string_to_table(1, ',');
SELECT string_to_table('x', 1);
SELECT generate_subscripts(1, 1);
SELECT generate_subscripts(ARRAY[1], true);
SELECT generate_subscripts(ARRAY[1], 1, 1);
SELECT pg_options_to_table(ARRAY[1]);
SELECT pg_get_sequence_data(true);
SELECT * FROM regexp_split_to_table(1, 'x');
SELECT * FROM string_to_table(1, ',');
SELECT * FROM generate_subscripts(1, 1);
SELECT * FROM pg_options_to_table(ARRAY[1]);
SELECT * FROM pg_get_sequence_data(true);
SELECT option_name, option_value
  FROM (VALUES (ARRAY['a=1','b=2'])) AS source(options)
 CROSS JOIN LATERAL pg_options_to_table(source.options)
 ORDER BY option_name;
CREATE SEQUENCE rows_from_sequence START WITH 7;
SELECT pg_get_sequence_data(oid)
  FROM pg_class WHERE relname = 'rows_from_sequence';
SELECT (pg_get_sequence_data(oid)).*
  FROM pg_class WHERE relname = 'rows_from_sequence';
SELECT nextval('rows_from_sequence');
SELECT (pg_get_sequence_data(oid)).*
  FROM pg_class WHERE relname = 'rows_from_sequence';
DROP SEQUENCE rows_from_sequence;

DROP VIEW rows_from_view;
DROP VIEW rows_from_target_view;
DROP MATERIALIZED VIEW rows_from_target_mat;
DROP FUNCTION rows_from_pair(integer);
DROP FUNCTION rows_from_schema.rows_from_values_moved(integer);
DROP SCHEMA rows_from_schema;
DROP FUNCTION rows_from_insert(integer);
DROP FUNCTION rows_from_empty();
DROP TABLE rows_from_effects;
DROP TABLE rows_from_target;
DROP TABLE rows_from_input;
