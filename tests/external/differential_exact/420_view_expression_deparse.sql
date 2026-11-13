-- pg_get_viewdef prints analyzed expressions as PostgreSQL's ruleutils does:
-- BETWEEN as its own AND list, LIKE ... ESCAPE through like_escape, SIMILAR
-- TO through similar_to_escape, prefix and binary operators with their
-- operand coercions, IN lists split into array and column comparisons, row
-- comparisons, SQL-syntax functions, subscripts, field selections, COLLATE,
-- simple CASE, bit and numeric constants, typmod casts, variadic and named
-- arguments and ordered-set aggregates.
CREATE TABLE t420 (a int, b text, c numeric, d date, ts timestamptz, j jsonb, arr int[], i8 bigint, f float8, by bytea, v varchar(10), q tsquery, tv tsvector, r int4range, bo bool);
CREATE TYPE comp420 AS (x int, y text);
CREATE TABLE tc420 (k comp420, ks comp420[]);
CREATE VIEW v420_1 AS SELECT a FROM t420 WHERE a BETWEEN 1 AND 3 AND b = 'x';
CREATE VIEW v420_2 AS SELECT a FROM t420 WHERE a NOT BETWEEN 1 AND 3 OR b = 'x';
CREATE VIEW v420_3 AS SELECT a FROM t420 WHERE b = 'x' AND a BETWEEN 1 AND 3;
CREATE VIEW v420_4 AS SELECT a FROM t420 WHERE a BETWEEN SYMMETRIC 1 AND 3;
CREATE VIEW v420_5 AS SELECT EXTRACT(year FROM d) AS y, EXTRACT(epoch FROM ts) AS e, ts AT TIME ZONE 'UTC' AS z, ts AT LOCAL AS l, (ts + interval '1 h') AT TIME ZONE 'UTC' AS z2 FROM t420;
CREATE VIEW v420_6 AS SELECT SUBSTRING(b FROM 2) AS s1, substring(b, 1, 2) AS s2, substring(b for 3) AS s3, substring(b similar 'a' escape '#') AS s4, trim(b) AS t1, trim(leading 'x' from b) AS t2, trim(trailing from b) AS t3, btrim(b) AS t4, position('a' in b) AS p, overlay(b placing 'z' from 1) AS o, normalize(b) AS n1, normalize(b, nfkc) AS n2, b IS NORMALIZED AS n3, b IS NOT NFD NORMALIZED AS n4 FROM t420;
CREATE VIEW v420_7 AS SELECT a FROM t420 WHERE a IN (1, a, 2) AND a IN (b::int) AND a NOT IN (1, 2) AND a IN (a, i8::int) AND b NOT IN (b, 'x');
CREATE VIEW v420_8 AS SELECT b LIKE 'x' ESCAPE '' AS l1, b NOT ILIKE 'x%' ESCAPE '!' AS l2, b SIMILAR TO 'x' ESCAPE '#' AS s1, b NOT SIMILAR TO 'y' AS s2, NOT (b SIMILAR TO 'z') AS s3, by LIKE 'x' AS l3, b LIKE ANY (ARRAY['a', 'b']) AS l4, b NOT ILIKE ALL ('{a,b}') AS l5 FROM t420;
CREATE VIEW v420_9 AS SELECT -a AS n1, - (a + 1) AS n2, @ c AS ab, |/ f AS sq, ||/ a AS cb, ~ i8 AS bn, !! q AS tn, a & 3 AS x1, i8 | a AS x2, a # 2 AS x3, i8 >> 2 AS x4, a ^ 2 AS x5, c ^ 2 AS x6, f ^ a AS x7 FROM t420;
CREATE VIEW v420_10 AS SELECT j -> 'a' AS j1, j ->> 0 AS j2, j #> '{a,b}' AS j3, j #>> '{a}' AS j4, j ? 'a' AS j5, j ?| ARRAY['a'] AS j6, j ?& '{a}' AS j7, j @> '{"a":1}' AS j8, '{}' <@ j AS j9, j #- '{a}' AS j10, j @? '$.a' AS j11, j @@ '$.a == 1' AS j12, j - 'a' AS j13, j || '{}' AS j14 FROM t420;
CREATE VIEW v420_11 AS SELECT tv @@ q AS m1, b @@ 'x' AS m2, q && q AS m3, q || q AS m4, q <-> q AS m5, r @> 3 AS r1, r && '[1,2)' AS r2, r -|- r AS r3, r &< r AS r4, r << r AS r5, r + r AS r6, arr @> ARRAY[1] AS a1, arr && '{1}' AS a2, arr || 4 AS a3, 0 || arr AS a4, arr || arr AS a5, b ^@ 'x' AS sw, b ~<~ 'y' AS pl FROM t420;
CREATE VIEW v420_12 AS SELECT arr[1] AS s1, arr[1:2] AS s2, arr[:2] AS s3, arr[2:] AS s4, (arr)[a] AS s5, ('{1}'::int[])[1] AS s6, (ARRAY[1,2])[1] AS s7 FROM t420;
CREATE VIEW v420_13 AS SELECT (k).x AS f1, (tc420.k).y AS f2, (ks[1]).x AS f3, k AS f4, ks[1] AS f5 FROM tc420;
CREATE VIEW v420_14 AS SELECT b COLLATE "C" AS c1, b COLLATE "POSIX" < 'x' AS c2, (b || 'x') COLLATE "C" AS c3 FROM t420 ORDER BY b COLLATE "C";
CREATE VIEW v420_15 AS SELECT CASE a WHEN 1 THEN 'x' WHEN 2 THEN 'y' END AS c1, CASE WHEN a > 1 THEN 1 ELSE 2.5 END AS c2, CASE b WHEN 'q' THEN a END AS c3 FROM t420;
CREATE VIEW v420_16 AS SELECT B'101' AS b1, X'1F' AS b2, B'1'::bit varying AS b3, 1.5 AS n1, 1e3 AS n2, -1.5 AS n3, 2147483648 AS n4, -2147483649 AS n5, -5 AS n6, 0.0 AS n7, 'x' AS t1, E'a\\b' AS t2, U&'d\0061t' AS t3, true AS t4, NULL AS t5, NULL::int AS t6 FROM t420;
CREATE VIEW v420_17 AS SELECT a::text AS c1, b::varchar(3) AS c2, c::numeric(10,2) AS c3, ts::timestamp(3) AS c4, b::char(2) AS c5, a::bigint::int AS c6, CAST(b AS int) AS c7, b::regclass AS c8, int '1' AS c9, '1'::int AS c10, a::float8 AS c11, v::text AS c12, b::varchar AS c13, d::timestamptz AS c14, interval '1' day AS c15, '1 day'::interval(0) AS c16, bit '1' AS c17, a::bool AS c18 FROM t420;
CREATE VIEW v420_18 AS SELECT (a, b) = (1, 'x') AS r1, (a, b) <> (1, 'x') AS r2, (a, b) < (1, 'x') AS r3, ROW(a, b) IS NULL AS r4, (a, b) IS NOT DISTINCT FROM (1, 'x') AS r5, (a, b) IN ((1, 'x'), (2, 'y')) AS r6 FROM t420;
CREATE VIEW v420_19 AS SELECT a IS NULL AS n1, bo IS UNKNOWN AS n2, bo IS NOT UNKNOWN AS n3, bo IS TRUE AS n4, a IS DISTINCT FROM 1 AS n5, a IS NOT DISTINCT FROM i8 AS n6, NOT bo AS n7, NOT (a > 1 AND bo) AS n8, (a > 1) = bo AS n9 FROM t420;
CREATE VIEW v420_20 AS SELECT a + 1 AS o1, a + i8 AS o2, a + c AS o3, a + f AS o4, c * f AS o5, a - 1.5 AS o6, d + 1 AS o7, d - d AS o8, ts - interval '1 day' AS o9, b || a AS o10, a || b AS o11, a % 3 AS o12, i8 / a AS o13, v || 'x' AS o14, v = 'x' AS o15, v = b AS o16 FROM t420;
CREATE VIEW v420_21 AS SELECT coalesce(a, i8) AS f1, nullif(b, '') AS f2, greatest(a, c) AS f3, least(1, 2) AS f4, length(b) AS f5, upper(v) AS f6, round(c, 2) AS f7, abs(a) AS f8, now() AS f9, to_char(d, 'YYYY') AS f10, array_length(arr, 1) AS f11, jsonb_build_object('a', a) AS f12, concat_ws(',', a, b) AS f13, md5(b) AS f14, date_trunc('day', ts) AS f15, generate_series(1, a) AS f16 FROM t420;
CREATE VIEW v420_22 AS SELECT count(*) AS a1, count(DISTINCT a) AS a2, sum(a) FILTER (WHERE a > 0) AS a3, string_agg(b, ',' ORDER BY b DESC) AS a4, array_agg(DISTINCT a ORDER BY a) AS a5, percentile_cont(0.5) WITHIN GROUP (ORDER BY c) AS a6, mode() WITHIN GROUP (ORDER BY a) AS a7, avg(c) AS a8, bool_and(bo) AS a9, GROUPING(a) AS a10 FROM t420 GROUP BY a;
CREATE VIEW v420_23 AS SELECT current_date AS s1, current_time(2) AS s2, localtimestamp AS s3, current_user AS s4, session_user AS s5, current_schema AS s6, current_catalog AS s7, current_role AS s9, user AS s10 FROM t420;
CREATE VIEW v420_24 AS SELECT ARRAY[1, 2] AS a1, ARRAY[a, 2] AS a2, ARRAY[[1,2],[3,4]] AS a3, ARRAY['x', b] AS a4, ARRAY[]::int[] AS a5, ARRAY(SELECT 1) AS a6, '{1,2}'::int[] AS a7, a = ANY ('{1,2}') AS a8, a = ANY (arr) AS a9, a > ALL (ARRAY[1]) AS a10, b = ANY ('{x}'::text[]) AS a11 FROM t420;
CREATE VIEW v420_25 AS SELECT format('%s', a) AS f1, concat(a, b, c) AS f2, jsonb_build_array(VARIADIC ARRAY[1,2]) AS f3, make_interval(days => 1) AS f4, make_interval(1, days => a) AS f5, num_nulls(a, b) AS f6 FROM t420;
CREATE VIEW v420_26 AS SELECT (SELECT max(a) FROM t420 t2 WHERE t2.a > t420.a) AS s1, a = (SELECT 1) AS s2, EXISTS (SELECT 1) AS s3, a IN (SELECT a FROM t420) AS s4, a NOT IN (SELECT 1) AS s5, a > ANY (SELECT 1) AS s6, a = ALL (SELECT 1) AS s7, ARRAY(SELECT a FROM t420) AS s8, NOT EXISTS (SELECT 1) AS s9 FROM t420;
CREATE VIEW v420_27 AS SELECT a::text::int AS c1, (a + 1)::text AS c2, (-a)::text AS c4, (a::numeric) AS c5, c::int AS c6, b::name AS c7, b::json AS c8, j::text AS c9, '{"a":1}'::json AS c10, 'a'::"char" AS c11, d::text AS c12, '2020-01-01'::date + 1 AS c13 FROM t420;
CREATE VIEW v420_28 AS SELECT a FROM t420 WHERE a > 1 AND (b = 'x' OR b = 'y') AND NOT bo AND (a < 5 AND c > 1);
CREATE VIEW v420_29 AS SELECT a FROM t420 WHERE (a > 1 OR b = 'x') AND (c > 1 OR (d IS NULL AND bo)) OR a = 5;
CREATE VIEW v420_30 AS SELECT 1 + 2 * 3 AS p1, (1 + 2) * 3 AS p2, a - (1 - 2) AS p3, a - 1 - 2 AS p4, -(-a) AS p5, a * -1 AS p6, (a > 1) IS TRUE AS p7, (a IS NULL) IS NOT NULL AS p8, 2 ^ 3 ^ 2 AS p9, a / 2 * 3 AS p10, a % 2 = 0 AS p11, b || 'x' || 'y' AS p12, b || ('x' || 'y') AS p13 FROM t420;
SELECT c.relname, pg_get_viewdef(c.oid) FROM pg_class c
  WHERE c.relkind = 'v' AND c.relname LIKE 'v420%' ORDER BY 1;
SELECT c.relname, pg_get_viewdef(c.oid, true) FROM pg_class c
  WHERE c.relkind = 'v' AND c.relname LIKE 'v420%' ORDER BY 1;
