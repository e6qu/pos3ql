-- Surface coverage: every expression form this engine claims to parse.
-- Values are asserted where they are stable; the point of the file is that
-- none of these becomes a syntax error again. A keyword-classification
-- change once turned the whole parenthesis-less family into 42601 with
-- nothing here to notice.

SELECT current_catalog;
SELECT current_schema;
SELECT CAST('1' AS int);
SELECT '1'::int;
SELECT EXTRACT(hour FROM TIMESTAMP '2020-01-01 05:00:00');
SELECT SUBSTRING('abcdef' FROM 2 FOR 3);
SELECT SUBSTRING('abcdef' FOR 3);
SELECT SUBSTRING('abcdef' FROM 'b.d');
SELECT TRIM(BOTH ' ' FROM '  x  ');
SELECT TRIM(LEADING 'x' FROM 'xxa');
SELECT TRIM(TRAILING 'x' FROM 'axx');
SELECT TRIM('  x  ');
SELECT POSITION('b' IN 'abc');
SELECT OVERLAY('abcdef' PLACING 'zz' FROM 2);
SELECT OVERLAY('abcdef' PLACING 'zz' FROM 2 FOR 3);
SELECT ARRAY[1,2,3];
SELECT ARRAY(SELECT 1);
SELECT ROW(1,2);
SELECT (1,2);
SELECT CASE WHEN true THEN 1 ELSE 2 END;
SELECT CASE 1 WHEN 1 THEN 'a' ELSE 'b' END;
SELECT COALESCE(NULL, 1);
SELECT NULLIF(1,1);
SELECT GREATEST(1,2), LEAST(1,2);
-- (the column label is `case` here, not `?column?` — B-102)
SELECT 1 IS DISTINCT FROM 2 AS d;
SELECT 1 IS NOT DISTINCT FROM 2 AS d;
SELECT NULL IS NULL, 1 IS NOT NULL;
SELECT (true IS TRUE) AS a, (true IS NOT FALSE) AS b, (NULL IS UNKNOWN) AS c;
SELECT TIMESTAMP '2020-01-01';
SELECT DATE '2020-01-01';
SELECT TIME '12:00:00';
SELECT INTERVAL '1 day';
SELECT TIMESTAMPTZ '2020-01-01 00:00:00+00';
SELECT B'101';
SELECT X'1F';
SELECT 'a' LIKE 'a%';
SELECT 'a' NOT LIKE 'b%';
SELECT 'a' ILIKE 'A%';
SELECT 'abc' SIMILAR TO 'a%';
SELECT 'abc' ~ 'a.c';
SELECT 1 BETWEEN 0 AND 2;
SELECT 1 NOT BETWEEN 0 AND 2;
SELECT 1 IN (1,2);
SELECT 1 = ANY(ARRAY[1,2]);
SELECT 1 = ALL(ARRAY[1,1]);
SELECT EXISTS (SELECT 1);
SELECT TIMESTAMP '2020-01-01 00:00:00' AT TIME ZONE 'UTC';
SELECT 1 AS x, 2 y;
SELECT DISTINCT 1;
SELECT DISTINCT ON (1) 1;
SELECT 1 UNION SELECT 2;
SELECT 1 INTERSECT SELECT 1;
SELECT 1 EXCEPT SELECT 2;
SELECT 1 ORDER BY 1 LIMIT 1 OFFSET 0;
SELECT 1 LIMIT ALL;
WITH c AS (SELECT 1 a) SELECT a FROM c;
WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<3) SELECT count(*) FROM c;
SELECT count(*) FILTER (WHERE true);
SELECT sum(1) OVER w FROM (SELECT 1) t WINDOW w AS ();
SELECT string_agg(x, ',' ORDER BY x) FROM (VALUES ('a')) v(x);
SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY x) FROM (VALUES (1.0)) v(x);
SELECT grouping(x) FROM (VALUES (1)) v(x) GROUP BY GROUPING SETS ((x));
SELECT 1 FROM (VALUES (1)) AS v(x);
SELECT * FROM generate_series(1,2) WITH ORDINALITY;
SELECT * FROM (SELECT 1) x CROSS JOIN (SELECT 2) y;
SELECT * FROM (SELECT 1 a) x JOIN (SELECT 1 a) y USING (a);
SELECT * FROM (SELECT 1 a) x NATURAL JOIN (SELECT 1 a) y;
-- window functions with no FROM: the single virtual row is a one-row source
SELECT row_number() OVER ();
SELECT rank() OVER (), dense_rank() OVER ();
SELECT sum(1) OVER (), count(*) OVER (), avg(2.5) OVER ();
SELECT sum(1) OVER (PARTITION BY 1 ORDER BY 1 ROWS BETWEEN 1 PRECEDING AND CURRENT ROW);
SELECT lag(5) OVER (), lead(5) OVER ();
SELECT first_value(7) OVER (), last_value(7) OVER (), nth_value(7,1) OVER ();
SELECT ntile(1) OVER (), percent_rank() OVER (), cume_dist() OVER ();
SELECT row_number() OVER () AS r, 1+1 AS x;
SELECT row_number() OVER () ORDER BY 1;
SELECT row_number() OVER () WHERE false;
SELECT (SELECT row_number() OVER ());
WITH c AS (SELECT row_number() OVER () n) SELECT n FROM c;
SELECT 1 IN (SELECT row_number() OVER ());
SELECT EXISTS (SELECT row_number() OVER ());
SELECT * FROM (SELECT row_number() OVER () n) x;
SELECT row_number() OVER () UNION SELECT 5 ORDER BY 1;
-- forms whose value moves, asserted by shape instead
-- (PostgreSQL types these as `name`; this engine has no such type — B-103)
SELECT user = current_user;
SELECT 1 LIMIT ALL;
SELECT 1 LIMIT ALL OFFSET 0;
CREATE TABLE sfc(a int DEFAULT 7, b text DEFAULT 'x');
INSERT INTO sfc DEFAULT VALUES RETURNING a, b;
INSERT INTO sfc DEFAULT VALUES;
SELECT count(*) FROM sfc;
DROP TABLE sfc;
-- array element quoting: PostgreSQL quotes an element that is empty, spells
-- `null`, or carries a comma, brace, quote, backslash or space — so a
-- timestamp comes out quoted, not only a string
-- (the column label of `ARRAY[...]::text` is `text` here, `array` in
--  PostgreSQL — same family as B-102)
SELECT ARRAY['a b','c,d','','null','q"r']::text AS a;
SELECT ARRAY[1,2]::text AS a, ARRAY['x','y']::text AS b, ARRAY[1.5,2.5]::text AS c, ARRAY[true,false]::text AS d;
SELECT ARRAY['2020-01-01'::date]::text AS a;
SELECT ARRAY['2020-01-01 00:00:00'::timestamp]::text AS a;
SELECT ARRAY['2020-01-01 00:00:00+00'::timestamptz]::text AS a;
SELECT array_agg(x)::text FROM (VALUES ('2020-01-01 12:00:00'::timestamp)) v(x);
SELECT array_agg(x)::text FROM (VALUES (1),(2)) v(x);
SELECT array_agg(x)::text FROM (VALUES ('a b'),('c')) v(x);
-- json has no equality operator in PostgreSQL: two documents differing only in
-- whitespace or key order are the same value but not the same text, so it
-- declines to say. jsonb, being canonicalized, does compare.
SELECT '{"a":1}'::json = '{"a":1}'::json;
SELECT '{"a":1}'::jsonb = '{"a":1}'::jsonb;
SELECT '{"a":1}'::json <> '{"a":2}'::json;
SELECT '{"a":1}'::jsonb <> '{"a":2}'::jsonb;
-- everything json can still do
SELECT '{"a":1}'::json->>'a', '{"a":1}'::json::text, ('{"a":1}'::json) IS NULL;
SELECT json_agg(x)::text FROM (VALUES (1),(2)) v(x);
SELECT count(x) FROM (VALUES ('{}'::json)) v(x);
