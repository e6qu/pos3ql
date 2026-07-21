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
