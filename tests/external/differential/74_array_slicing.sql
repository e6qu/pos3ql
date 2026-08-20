-- Array slice subscripting `a[lo:hi]` (with either bound optional), matching
-- PostgreSQL 18. 1-based and inclusive; bounds clamp to the array, an empty
-- overlap is an empty array, and a NULL bound yields NULL.
DROP TABLE IF EXISTS asl;

-- Both bounds, and each bound omitted.
SELECT (ARRAY[1,2,3,4,5])[2:4];
SELECT (ARRAY[1,2,3,4,5])[:3];
SELECT (ARRAY[1,2,3,4,5])[3:];
SELECT (ARRAY[1,2,3,4,5])[:];

-- Out-of-range bounds clamp; a lower bound below 1 clamps to 1.
SELECT (ARRAY[1,2,3])[2:10];
SELECT (ARRAY[1,2,3])[0:2];
SELECT (ARRAY[1,2,3])[5:10];
SELECT (ARRAY[1,2,3])[3:1];

-- A NULL bound makes the whole slice NULL.
SELECT (ARRAY[1,2,3])[NULL:2] IS NULL;

-- Works on text arrays, and the result keeps the array type.
SELECT (ARRAY['a','b','c','d'])[2:3];
SELECT pg_typeof((ARRAY[1,2,3])[1:2]);
SELECT array_length((ARRAY[1,2,3,4,5])[2:4], 1);

-- Rectangular arrays retain every dimension and lower bound through text input,
-- constructors, subscripting, slicing, comparison, and dimensional functions.
SELECT '{{1,2},{3,4}}'::int[];
SELECT ('{{1,2},{3,4}}'::int[])[2][1];
SELECT array_ndims('{{1,2},{3,4}}'::int[]), array_length('{{1,2},{3,4}}'::int[], 2), cardinality('{{1,2},{3,4}}'::int[]);
SELECT array_dims('[2:3][4:5]={{1,2},{3,4}}'::int[]), array_lower('[2:3][4:5]={{1,2},{3,4}}'::int[], 1), array_upper('[2:3][4:5]={{1,2},{3,4}}'::int[], 2);
SELECT ('[2:3][4:5]={{1,2},{3,4}}'::int[])[3][5], ('[2:3][4:5]={{1,2},{3,4}}'::int[])[2:2];
SELECT array_fill(7, ARRAY[2,3], ARRAY[4,8]);

-- array_agg(array) appends a leading dimension; arrays cannot be represented
-- as independent array elements.
SELECT array_agg(a) FROM (VALUES (ARRAY[1,2]), (ARRAY[3,4])) AS agg_array(a);
SELECT ARRAY(SELECT a FROM (VALUES (ARRAY[1,2]), (ARRAY[3,4])) AS array_subquery(a));
SELECT ARRAY(SELECT a FROM (VALUES (ARRAY[1]::int[])) AS array_empty(a) WHERE false);
SELECT array_agg(a) FROM (VALUES ('[2:3]={1,2}'::int[]), ('[2:3]={3,4}'::int[])) AS agg_bounds(a);
SELECT array_agg(NULL);
SELECT array_agg(a) FROM (VALUES (ARRAY[1,2]), (ARRAY[3])) AS agg_mismatch(a);
SELECT array_agg(a) FROM (VALUES (ARRAY[]::int[])) AS agg_empty(a);
SELECT array_agg(a) FROM (VALUES (NULL::int[])) AS agg_null(a);
SELECT ARRAY(SELECT a FROM (VALUES (ARRAY[]::int[])) AS subquery_empty(a));
SELECT ARRAY(SELECT a FROM (VALUES (NULL::int[])) AS subquery_null(a));
SELECT ARRAY[ARRAY[1,2], ARRAY[3,4]];
SELECT ARRAY[ARRAY[1,2], ARRAY[3,4]] = '{{1,2},{3,4}}'::int[];

-- ARRAY(subquery) retains a named composite element identity even when the
-- subquery is empty; the result is usable for durable CTAS output too.
CREATE TYPE array_subquery_pair AS (x integer, y text);
CREATE TABLE array_subquery_pairs (value array_subquery_pair);
INSERT INTO array_subquery_pairs VALUES (ROW(1, 'one')::array_subquery_pair);
SELECT ARRAY(SELECT value FROM array_subquery_pairs)::text,
       pg_typeof(ARRAY(SELECT value FROM array_subquery_pairs))::text;
SELECT ARRAY(SELECT value FROM array_subquery_pairs WHERE false)::text,
       pg_typeof(ARRAY(SELECT value FROM array_subquery_pairs WHERE false))::text;
SELECT ((ARRAY(SELECT value FROM array_subquery_pairs))[1]).x;
CREATE TABLE array_subquery_pairs_copy AS
  SELECT ARRAY(SELECT value FROM array_subquery_pairs) AS values;
SELECT values::text, pg_typeof(values)::text FROM array_subquery_pairs_copy;
DROP TABLE array_subquery_pairs_copy;
DROP TABLE array_subquery_pairs;
DROP TYPE array_subquery_pair;

-- Composes with || and other array functions.
SELECT (ARRAY[1,2,3])[1:2] || (ARRAY[4,5])[1:1];
SELECT array_agg(x) FROM unnest((ARRAY[10,20,30,40])[2:3]) AS x;

-- On a column, alongside a plain index.
CREATE TABLE asl (a int[]);
INSERT INTO asl VALUES (ARRAY[10,20,30,40]);
SELECT a[2:3], a[1], a[2:] FROM asl;

DROP TABLE asl;
