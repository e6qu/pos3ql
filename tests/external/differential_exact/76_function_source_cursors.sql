-- Cursors over builtin set-returning functions with constant arguments
-- produce each row when it is fetched: regex, JSON, ROWS FROM and
-- ordinality sources keep only their position between FETCHes.
CREATE SEQUENCE function_source_calls START 10;
BEGIN;
DECLARE matches NO SCROLL CURSOR FOR
  SELECT m, nextval('function_source_calls')
    FROM regexp_matches('k1=v1;k2=;k3=v3', '(\w+)=(\w*)', 'g') m;
SELECT last_value, is_called FROM function_source_calls;
FETCH 1 matches;
SELECT last_value, is_called FROM function_source_calls;
FETCH ALL matches;
DECLARE pieces SCROLL CURSOR FOR
  SELECT * FROM regexp_split_to_table('a1b22c', '\d+') WITH ORDINALITY;
FETCH ALL pieces;
FETCH BACKWARD 2 pieces;
DECLARE members NO SCROLL CURSOR FOR
  SELECT key, value FROM jsonb_each('{"z": {"b": 1, "a": [1, 2.50]}, "a": null, "z": true}');
FETCH ALL members;
DECLARE source_members NO SCROLL CURSOR FOR
  SELECT key, value FROM json_each_text('{"z": "q\"uote", "a" : [1,  2], "z": 1e2}');
FETCH ALL source_members;
COMMIT;
BEGIN;
DECLARE lockstep NO SCROLL CURSOR FOR
  SELECT * FROM ROWS FROM (json_array_elements('[1, {"a": "b"}]'),
                           jsonb_object_keys('{"y": 1, "x": 2, "xx": 3}'),
                           regexp_split_to_table('', ','))
         WITH ORDINALITY AS t(element, key, piece, n);
FETCH ALL lockstep;
DECLARE empty_keys NO SCROLL CURSOR FOR SELECT * FROM json_object_keys('{}');
FETCH ALL empty_keys;
DECLARE null_input NO SCROLL CURSOR FOR SELECT * FROM jsonb_array_elements_text(NULL);
FETCH ALL null_input;
COMMIT;
-- Argument errors are raised by the first FETCH, before any row.
BEGIN;
DECLARE bad_each NO SCROLL CURSOR FOR SELECT * FROM jsonb_each('[1]');
FETCH 1 bad_each;
ROLLBACK;
BEGIN;
DECLARE bad_each_text NO SCROLL CURSOR FOR SELECT * FROM json_each_text('3');
FETCH 1 bad_each_text;
ROLLBACK;
BEGIN;
DECLARE bad_keys NO SCROLL CURSOR FOR SELECT * FROM json_object_keys('[1]');
FETCH 1 bad_keys;
ROLLBACK;
BEGIN;
DECLARE bad_elements NO SCROLL CURSOR FOR SELECT * FROM jsonb_array_elements('{"a": 1}');
FETCH 1 bad_elements;
ROLLBACK;
BEGIN;
DECLARE bad_split NO SCROLL CURSOR FOR SELECT * FROM regexp_split_to_table('abc', 'b', 'g');
FETCH 1 bad_split;
ROLLBACK;
BEGIN;
DECLARE bad_pattern NO SCROLL CURSOR FOR SELECT * FROM regexp_matches('abc', '(');
FETCH 1 bad_pattern;
ROLLBACK;
SELECT * FROM jsonb_each('1');
SELECT * FROM jsonb_each_text('[1]');
SELECT * FROM json_each('[1]');
-- Malformed patterns name PostgreSQL's regex error.
SELECT 'x' ~ 'a{2';
SELECT 'x' ~ 'a{2,3';
SELECT 'x' ~ 'a{2x}';
SELECT 'x' ~ '[a';
SELECT 'x' ~ 'a)';
SELECT 'x' ~ 'a\';
SELECT regexp_matches('a\', 'a\', 'x');
SELECT regexp_matches('a\', 'a\', 'q');
DROP SEQUENCE function_source_calls;
