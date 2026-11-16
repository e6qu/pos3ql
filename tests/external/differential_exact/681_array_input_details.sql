-- A malformed array literal reports PostgreSQL's DETAIL (array_in).
SELECT '{1'::int[];
SELECT '1'::int[];
SELECT '{{1},{1,2}}'::int[];
SELECT '{1}x'::int[];
SELECT '{1,{2}}'::int[];
SELECT '[1:2]={1}'::int[];
SELECT '{'::int[];
SELECT '{"a}'::text[];
SELECT '[1:a]={1}'::int[];
SELECT '[2:1]={}'::int[];
SELECT '[1:2'::int[];
SELECT '[1:2]{1,2}'::int[];
SELECT '[1:2]=1'::int[];
SELECT '{{1},2}'::int[];
SELECT '{1,,2}'::int[];
SELECT '{,1}'::int[];
SELECT '{1,}'::int[];
SELECT '{}}'::int[];
SELECT '{{1}{2}}'::int[];
SELECT '{"a"b}'::text[];
SELECT '{a"b"}'::text[];
SELECT '{\'::text[];
SELECT '{a,b'::text[];
SELECT '[+]={1}'::int[];
SELECT '[1:]={1}'::int[];
SELECT '[99999999999:1]={1}'::int[];
SELECT '[-2147483648:2147483646]={1}'::int[];
SELECT '[0:2147483647]={1}'::int[];
SELECT '[1:2][3:4]={{1}}'::int[];
SELECT '[1:2]={1,2,3}'::int[];
SELECT '{{{{{{{1}}}}}}}'::int[];
-- An element's input error comes first when it precedes the syntax error,
-- as PostgreSQL converts each element as it reads it.
SELECT '{x}'::int[];
SELECT '{x,}'::int[];
SELECT '{{1,x},{2}}'::int[];
SELECT '{1,{2},x}'::int[];
-- Accepted forms: whitespace, NULL, quoting, escapes, bounds and delimiters.
SELECT '{a b}'::text[], '{ {1} , {2} }'::int[], '{{}}'::int[], '{{},{}}'::int[];
SELECT '{1,NULL,null,"NULL"}'::int[], '{"a b", c d ,\"e}'::text[], '{  a  ,  b\  }'::text[];
SELECT '[0:1]={1,2}'::int[], '[2]={7,8}'::int[], ' [1:1] = {5} '::int[];
SELECT '{(1,2),(3,4);(5,6),(7,8)}'::box[];
SELECT '{1}'::int[][], '{{1,2},{3,4}}'::int[];
SELECT array_length('{"\\x",b}'::text[], 1), '{"\\x"}'::bytea[];
-- A soft input check reports the DETAIL.
SELECT * FROM pg_input_error_info('{1', 'int[]');
SELECT * FROM pg_input_error_info('{1,{2}}', 'int[]');
SELECT * FROM pg_input_error_info('{x}', 'int[]');
-- Context comes from the frames the literal is read in.
DO $$ DECLARE x int[]; BEGIN x := '{1,2'; END $$;
CREATE TABLE array_rows (a int[]);
COPY array_rows FROM stdin;
{1,{2}}
\.
DROP TABLE array_rows;
