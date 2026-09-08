-- PostgreSQL 18 SQL/JSON: first-class jsonpath values, path execution,
-- standard constructors and queries, JSON_TABLE, record conversion, and
-- jsonb subscripting through stored and data-modifying query boundaries.
SET TimeZone = 'UTC';

SELECT oid, typname, typtype, typcategory, typelem, typarray
  FROM pg_type WHERE oid IN (4072, 4073) ORDER BY oid;
SELECT oid, proname, prorettype, proargtypes, pronargdefaults,
       proretset, provolatile, proisstrict
  FROM pg_proc
 WHERE oid IN (1179, 3204, 3205, 3209, 3475, 3490, 3491,
               3960, 3961, 4005, 4006, 4007, 4008, 4009, 6338)
 ORDER BY oid;
SELECT oid, oprname, oprleft, oprright, oprresult, oprcode::oid
  FROM pg_operator WHERE oid IN (4012, 4013) ORDER BY oid;

CREATE TABLE sql_json_paths (
  id integer PRIMARY KEY,
  path jsonpath,
  paths jsonpath[]
);
INSERT INTO sql_json_paths VALUES
  (1, 'lax $.items[*] ? (@.price >= 10)',
      ARRAY['$.name'::jsonpath, 'strict $.items[0]'::jsonpath]),
  (2, '$var + $.amount', ARRAY[]::jsonpath[]);
SELECT id, path, paths, pg_typeof(path), pg_typeof(paths)
  FROM sql_json_paths ORDER BY id;

SELECT jsonb_path_query_array(
         '{"items":[{"name":"a","price":8},{"name":"b","price":12}],"amount":7}',
         '$.items[*] ? (@.price >= 10).name'),
       jsonb_path_query_first('[10,20,30]', '$[last - 1]'),
       jsonb_path_exists('{"a":1}', '$.a'),
       jsonb_path_match('{"a":2}', '$.a == 2');
SELECT jsonb_path_query_array('{"a":{"b":[1,2]}}', '$.**'),
       jsonb_path_query_array('[1,2,3,4]', '$[1 to last - 1]'),
       jsonb_path_query_array('{"x":7}', '$v + $.x', '{"v":5}');
SELECT '{"a":[1,2,3]}'::jsonb @? '$.a[*] ? (@ == 2)',
       '{"a":2}'::jsonb @@ '$.a == 2',
       '{"a":2}'::jsonb @? 'strict $.missing';
SELECT jsonb_path_query_array('["Abc","a\nb"]',
         '$[*] ? (@ starts with "A" || @ like_regex "^a.b$" flag "si")'),
       jsonb_path_query_array('[1,2,3]', '$[*].double()'),
       jsonb_path_query_array('[1.25,-2.5]', '$[*].decimal(4,1)');
SELECT jsonb_path_query('1.5', '$.bigint()'),
       jsonb_path_query('-1.5', '$.integer()'),
       jsonb_path_query('"12:34:56.789 +05:30"', '$.time_tz(2)'),
       jsonb_path_query('"2023-08-15 12:34:56.789"', '$.timestamp(2)');
SELECT jsonb_path_query_array('[{"a":1},{"b":2}]', '$[*].keyvalue()');
SELECT jsonb_path_query_array('["bad"]', '$[*].date()', '{}'::jsonb, true),
       jsonb_path_query_array('{"a":1}', 'strict $.missing', '{}'::jsonb, true);
SELECT jsonb_path_query_array(
         '[-1.2,1.2,[1,2],{"a":1},true,null]', '$[*].type()'),
       jsonb_path_query_array('[-1.2,1.2]', '$[*].abs()'),
       jsonb_path_query_array('[-1.2,1.2]', '$[*].floor()'),
       jsonb_path_query_array('[-1.2,1.2]', '$[*].ceiling()');
SELECT jsonb_path_query_array('[[1,2],{"a":1},1]', '$[*].size()'),
       jsonb_path_query_array('[1,true,"x"]', '$[*].string()'),
       jsonb_path_query_array('["1.25","-2"]', '$[*].number()');
SELECT jsonb_path_exists_tz(
         '{}', '"2024-01-01 01:00:00+02".timestamp_tz() <
                "2024-01-01 00:00:00+00".timestamp_tz()'),
       jsonb_path_match_tz(
         '{}', '"12:00:00+02".time_tz() == "10:00:00+00".time_tz()'),
       jsonb_path_query_array_tz(
         '["2024-01-01 00:00:00+00"]', '$[*].timestamp_tz()');
SELECT jsonb_path_query_array('{}', 'strict $.missing');
SELECT jsonb_path_query_array('1', 'strict $.*');
SELECT jsonb_path_query_array('1', 'strict $[*]');
SELECT jsonb_path_query_array('[1]', 'strict $[2]');
SELECT jsonb_path_query_array('[null]', '$[*].string()');
SELECT jsonb_path_query_array('["bad"]', '$[*].date()');

CREATE SEQUENCE sql_json_evaluation_sequence;
SELECT jsonb_path_query(
         jsonb_build_array(nextval('sql_json_evaluation_sequence'),
                           nextval('sql_json_evaluation_sequence')), '$[*]');
SELECT last_value FROM sql_json_evaluation_sequence;
ALTER SEQUENCE sql_json_evaluation_sequence RESTART WITH 1;
SELECT value FROM jsonb_path_query(
       jsonb_build_array(nextval('sql_json_evaluation_sequence'),
                         nextval('sql_json_evaluation_sequence')), '$[*]') AS rows(value);
SELECT last_value FROM sql_json_evaluation_sequence;

SELECT '1' IS JSON VALUE, '1' IS JSON SCALAR, '[]' IS JSON ARRAY,
       '{}' IS JSON OBJECT,
       '{"a":1,"a":2}' IS JSON WITH UNIQUE KEYS,
       '{"a":{"b":1,"b":2}}' IS JSON WITHOUT UNIQUE KEYS;
SELECT JSON(' {"b":2,"a":1,"b":3} '), JSON_SCALAR(12.50),
       JSON_SCALAR('x'), JSON_SERIALIZE(JSON('{"a":1}') RETURNING text);
SELECT JSON_ARRAY(1, NULL, 'x' NULL ON NULL RETURNING jsonb),
       JSON_ARRAY(1, NULL, 'x' ABSENT ON NULL),
       JSON_OBJECT('a' VALUE 1, 'b' VALUE NULL NULL ON NULL),
       JSON_OBJECT('a' VALUE 1, 'b' VALUE NULL ABSENT ON NULL
                   WITH UNIQUE KEYS RETURNING jsonb);
SELECT JSON_ARRAYAGG(value ORDER BY id NULL ON NULL),
       JSON_ARRAYAGG(value ORDER BY id ABSENT ON NULL RETURNING jsonb)
  FROM (VALUES (2, NULL), (1, 7), (3, 9)) AS source(id, value);
SELECT JSON_OBJECTAGG(key VALUE value ABSENT ON NULL
                      WITH UNIQUE KEYS RETURNING jsonb)
  FROM (VALUES (1, 'a', 7), (2, 'b', NULL)) AS source(id, key, value);

SELECT JSON_EXISTS('{"items":[1,2,3]}', '$.items[*] ? (@ == $wanted)'
                   PASSING 2 AS wanted),
       JSON_VALUE('{"a":"12"}', '$.a' RETURNING integer),
       JSON_VALUE('{}', '$.a' RETURNING integer DEFAULT 7 ON EMPTY),
       JSON_VALUE('{"a":"bad"}', '$.a' RETURNING integer DEFAULT 9 ON ERROR);
SELECT JSON_QUERY('{"a":[1,2]}', '$.a' RETURNING jsonb),
       JSON_QUERY('{"a":[1,2]}', '$.a[*]' WITH ARRAY WRAPPER),
       JSON_QUERY('{"a":"x"}', '$.a' RETURNING text OMIT QUOTES),
       JSON_QUERY('{}', '$.a' EMPTY ARRAY ON EMPTY);

SELECT * FROM JSON_TABLE(
  '{"groups":[{"kind":"film","items":[{"name":"A"},{"name":"B"}]},
              {"kind":"book","items":[]}]}'::jsonb,
  '$.groups[*]' AS group_path
  COLUMNS (
    group_number FOR ORDINALITY,
    kind text,
    has_items boolean EXISTS PATH '$.items[*]',
    names text FORMAT JSON PATH '$.items[*].name' WITH ARRAY WRAPPER,
    NESTED PATH '$.items[*]' AS item_path COLUMNS (
      item_number FOR ORDINALITY,
      name text,
      missing integer DEFAULT 7 ON EMPTY
    )
  )
) AS table_rows;
SELECT * FROM JSON_TABLE('{}', '$'::jsonpath COLUMNS (value text)) AS table_rows;
SELECT * FROM JSON_TABLE('{}', concat('$', '') COLUMNS (value text)) AS table_rows;
SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (
  NESTED PATH concat('$', '') COLUMNS (value text)
)) AS table_rows;

CREATE TYPE sql_json_child AS (count integer, label text);
CREATE TYPE sql_json_row AS (
  id integer,
  tags text[],
  child sql_json_child,
  matrix integer[][]
);
CREATE DOMAIN sql_json_positive_row AS sql_json_child
  CHECK ((VALUE).count > 0);
SELECT * FROM jsonb_populate_record(
  NULL::sql_json_row,
  '{"id":1,"tags":["x","a b"],"child":{"count":3,"label":"ok"},"matrix":[[1,2],[3,4]]}'
);
SELECT (json_populate_record(NULL::sql_json_row, '{"id":2}', false)).*;
SELECT * FROM jsonb_populate_recordset(
  NULL::sql_json_row, '[{"id":3},{"id":4,"tags":["z"]}]'
);
SELECT * FROM json_to_record('{"id":5,"label":"five"}')
  AS record_value(id integer, label varchar(8));
SELECT * FROM jsonb_to_recordset('[{"id":6},{"id":7,"label":"seven"}]')
  AS record_value(id integer, label text);
SELECT jsonb_populate_record_valid(NULL::sql_json_row, '{"id":8}'),
       jsonb_populate_record_valid(NULL::sql_json_row, '{"id":"bad"}'),
       jsonb_populate_record_valid(NULL::sql_json_row, NULL);
ALTER SEQUENCE sql_json_evaluation_sequence RESTART WITH 1;
SELECT * FROM jsonb_populate_recordset(
  ROW(nextval('sql_json_evaluation_sequence')::integer, NULL, NULL, NULL)::sql_json_row,
  '[{},{}]');
SELECT last_value FROM sql_json_evaluation_sequence;
SELECT pg_typeof(jsonb_populate_record(
         NULL::sql_json_positive_row, '{"count":3,"label":"domain"}')),
       jsonb_populate_record(
         NULL::sql_json_positive_row, '{"count":3,"label":"domain"}');
SELECT * FROM jsonb_populate_recordset(
  NULL::sql_json_positive_row, '[{"count":4},{"count":5,"label":"five"}]');

CREATE TABLE sql_json_documents (id integer PRIMARY KEY, document jsonb);
INSERT INTO sql_json_documents VALUES (1, NULL), (2, '{"items":[1,2]}');
UPDATE sql_json_documents
   SET document['profile']['name'] = '"Ada"'::jsonb,
       document['items'][4] = '9'::jsonb
 WHERE id = 1
 RETURNING id, document;
UPDATE sql_json_documents
   SET document['items'][-1] = '7'::jsonb
 WHERE id = 2
 RETURNING id, document, document['items'][0], document['items'][-1];

CREATE FUNCTION sql_json_document_trigger() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  NEW.document['triggered']['values'][1] := '7'::jsonb;
  RETURN NEW;
END $$;
CREATE TRIGGER sql_json_document_trigger
  BEFORE INSERT OR UPDATE ON sql_json_documents
  FOR EACH ROW EXECUTE FUNCTION sql_json_document_trigger();
CREATE FUNCTION sql_json_local_assignment(input jsonb) RETURNS jsonb
LANGUAGE plpgsql AS $$
BEGIN
  input['local']['value'] := '9'::jsonb;
  input['local']['items'][0] := 'true'::jsonb;
  RETURN input;
END $$;
INSERT INTO sql_json_documents VALUES (3, NULL);
UPDATE sql_json_documents SET document['updated'] = 'true'::jsonb WHERE id = 3;
SELECT id, document, sql_json_local_assignment('{}')
  FROM sql_json_documents WHERE id = 3;

CREATE VIEW sql_json_view AS
  SELECT source.id, table_row.ordinality, table_row.value
    FROM sql_json_documents AS source,
         LATERAL JSON_TABLE(source.document, '$.items[*]'
           COLUMNS (ordinality FOR ORDINALITY, value integer PATH '$')) AS table_row;
SELECT * FROM sql_json_view ORDER BY id, ordinality;
CREATE MATERIALIZED VIEW sql_json_materialized AS
  SELECT * FROM sql_json_view;
SELECT * FROM sql_json_materialized ORDER BY id, ordinality;
CREATE TABLE sql_json_ctas AS
  SELECT * FROM JSON_TABLE('[{"id":10},{"id":11}]', '$[*]'
    COLUMNS (id integer)) AS rows;
SELECT * FROM sql_json_ctas ORDER BY id;

CREATE TABLE sql_json_dml (id integer PRIMARY KEY, label text);
INSERT INTO sql_json_dml
  SELECT id, label FROM JSON_TABLE(
    '[{"id":1,"label":"one"},{"id":2,"label":"two"}]', '$[*]'
    COLUMNS (id integer, label text)) AS source;
UPDATE sql_json_dml AS target SET label = source.label
  FROM JSON_TABLE('[{"id":2,"label":"TWO"}]', '$[*]'
    COLUMNS (id integer, label text)) AS source
  WHERE target.id = source.id;
DELETE FROM sql_json_dml AS target
  USING JSON_TABLE('[1]', '$[*]' COLUMNS (id integer PATH '$')) AS source
  WHERE target.id = source.id;
MERGE INTO sql_json_dml AS target
  USING JSON_TABLE(
    '[{"id":2,"label":"second"},{"id":3,"label":"three"}]', '$[*]'
    COLUMNS (id integer, label text)) AS source
  ON target.id = source.id
  WHEN MATCHED THEN UPDATE SET label = source.label
  WHEN NOT MATCHED THEN INSERT (id, label) VALUES (source.id, source.label);
CREATE FUNCTION sql_json_table_plpgsql(input jsonb)
  RETURNS TABLE (id integer, label text) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT source.id, source.label
    FROM JSON_TABLE(input, '$[*]'
      COLUMNS (id integer, label text)) AS source;
END $$;
SELECT * FROM sql_json_dml ORDER BY id;
SELECT * FROM sql_json_table_plpgsql(
  '[{"id":4,"label":"four"},{"id":5,"label":"five"}]');

BEGIN;
DECLARE sql_json_cursor CURSOR FOR
  SELECT value FROM jsonb_path_query('[1,2,3]', '$[*]') AS result(value);
FETCH 2 FROM sql_json_cursor;
FETCH ALL FROM sql_json_cursor;
COMMIT;
PREPARE sql_json_prepared(jsonb, jsonpath) AS
  SELECT jsonb_path_query_array($1, $2);
EXECUTE sql_json_prepared('{"a":[1,2]}', '$.a[*]');
DEALLOCATE sql_json_prepared;
COPY (SELECT id, path FROM sql_json_paths ORDER BY id) TO STDOUT;

DROP MATERIALIZED VIEW sql_json_materialized;
DROP VIEW sql_json_view;
DROP FUNCTION sql_json_table_plpgsql(jsonb);
DROP FUNCTION sql_json_local_assignment(jsonb);
DROP TABLE sql_json_dml;
DROP TABLE sql_json_ctas;
DROP TABLE sql_json_documents;
DROP TABLE sql_json_paths;
DROP TYPE sql_json_row;
DROP DOMAIN sql_json_positive_row;
DROP TYPE sql_json_child;
DROP SEQUENCE sql_json_evaluation_sequence;
