-- Invalid json and jsonb input reports PostgreSQL's DETAIL (json_errdetail)
-- and its `JSON data, line N:` CONTEXT, which shows the line up to where the
-- error was found, cut to about 50 bytes with `...` marking the cut ends.
SELECT '{"a":1,}'::json;
SELECT '{"a" 1}'::jsonb;
SELECT '[1,2'::json;
SELECT 'tru'::json;
SELECT 'nul'::jsonb;
SELECT '"\x"'::json;
SELECT '"\u12"'::json;
SELECT E'"abc\x01"'::json;
SELECT '01'::json, 1;
SELECT '1.'::jsonb;
SELECT '-'::jsonb;
SELECT '1e'::jsonb;
SELECT '1abc'::json;
SELECT '[1 2]'::jsonb;
SELECT '{1:2}'::jsonb;
SELECT '{"a":1 "b"}'::jsonb;
SELECT ''::jsonb;
SELECT '  '::json;
SELECT '[,]'::json;
SELECT '[1,]'::json;
SELECT '{"a"}'::json;
SELECT '{"a":}'::json;
SELECT '[1]]'::json;
SELECT 'ä'::json;
SELECT '@'::json;
SELECT '"unterminated'::json;
-- Unicode escapes: json keeps them undecoded; jsonb decodes and checks them.
SELECT '"\ud800"'::json, '"\u0000"'::json, '"é😀"'::jsonb;
SELECT '"\ud800"'::jsonb;
SELECT '"\udc00"'::jsonb;
SELECT '"\u0000"'::jsonb;
SELECT '"\ud800\ud800"'::jsonb;
SELECT '"\ud800x"'::jsonb;
SELECT '"\ud800\n"'::jsonb;
-- The context window: line numbers, a cut start and a cut end.
SELECT E'{"a":\n 1, "b": \n [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20 x]}'::json;
SELECT E'[1,\n2,\n3,\n  foo]'::json;
SELECT E'[1,\r\n2,\r\nfoo]'::json;
SELECT E'{"a":\t\t1,\tb}'::json;
SELECT '{"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa": ä}'::json;
SELECT '{"a": 1, "b": {"c": [1, 2, {"d": tru}]}}'::jsonb;
-- The context follows the error into a routine's frames.
DO $$ BEGIN PERFORM '{x'::jsonb; END $$;
-- A soft input check reports the DETAIL, and no context.
SELECT * FROM pg_input_error_info('{"a":1,}', 'json');
SELECT * FROM pg_input_error_info('"\u0000"', 'jsonb');
SELECT pg_input_is_valid('[', 'jsonb'), '[1'::jsonb;
SELECT '"\ud800"' IS JSON, '"\ud800"' IS JSON WITH UNIQUE KEYS, '{"a":1,"a":2}' IS JSON WITH UNIQUE KEYS, 'x' IS JSON;
-- Processing functions check escapes as PostgreSQL's do: the json `_text`
-- element functions, objects and accessors decode them; the others do not.
SELECT json_array_elements('[1,"\ud800"]');
SELECT json_array_elements_text('[1,"\ud800"]');
SELECT json_array_length('[1, "\ud800", [2]]');
SELECT json_object_keys('{"a":"\u0000"}');
SELECT json_object_keys('"\ud800"'::json);
SELECT json_each('{"a":"\ud800x"}');
SELECT '{"a":"\u0000"}'::json->>'a';
SELECT '[1, "\u0000"]'::json->0;
-- A json accessor returns its member's source text.
SELECT '{"a":1,"a": [2,  3]}'::json->'a';
SELECT '[1, [2,  3] , 4]'::json->1, '[1, [2,  3] , 4]'::json->-1, '[1, [2,  3] , 4]'::json->-2, '[1,2]'::json->5;
SELECT '{"a":{"b": [1,  {"c" : "x\ty"}]}}'::json #> '{a,b,1}', '{"a":{"b": [1,  {"c" : "x\ty"}]}}'::json #>> '{a,b,1,c}';
SELECT '{"a":{"b": [1,  2]}}'::json #> '{a,b,-1}', '{"a":"s"}'::json #> '{}', '{"a":"s"}'::json #>> '{}';
SELECT '{"a":null}'::json->>'a', '{"a":null}'::json->'a', '{"a":true}'::json->>'a';
SELECT ' "x" '::json #> '{}', ' "x" '::json #>> '{}', '[1]'::json #> '{x}', '{"a":1}'::json #> '{a,b}';
SELECT '{"a":"x"}'::json->1, '[1]'::json->'a', '"s"'::json->'a';
SELECT json_array_length('{}');
SELECT json_array_length('1');
SELECT jsonb_array_length('{}');
-- Numbers keep their whole canonical form; a path element must be an integer
-- where it indexes an array.
SELECT '1e100'::jsonb, '-0'::jsonb, '1E+2'::jsonb, '0.000'::jsonb, '-1.5e-3'::jsonb;
SELECT '1e1000000'::jsonb;
SELECT length(('[' || repeat('"abcdefghij",', 2000) || '1]')::jsonb::text);
SELECT length((('{"a": [' || repeat('1234567890,', 1000) || '1]}')::jsonb -> 'a')::text);
SELECT jsonb_insert('[1,2]', '{a}', '3');
SELECT jsonb_insert('{"x":[1,2]}', '{x,b}', '3');
SELECT '[1,2]'::jsonb #- '{a}';
SELECT '{"x":[1,2]}'::jsonb #- '{x,99999999999}';
SELECT jsonb_set('[1,2]', '{a}', '3');
SELECT jsonb_set('{"x":[1,2]}', '{x, 1 }', '3'), jsonb_set('{"x":[1,2]}', '{x,+1}', '3');
-- COPY reports the JSON context before its own line.
CREATE TABLE json_rows (j json, b jsonb);
COPY json_rows (j) FROM stdin;
{"a":
\.
COPY json_rows (b) FROM stdin;
[1, "\\ud800"]
\.
DROP TABLE json_rows;
