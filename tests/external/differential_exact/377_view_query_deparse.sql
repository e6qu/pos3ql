-- pg_get_viewdef, in both modes and with a wrap column, and pg_views print
-- PostgreSQL's deparsed form of views over subqueries, functions, explicit
-- joins, set operations, DISTINCT ON, windows, grouping sets, WITH queries,
-- LATERAL items, VALUES lists and TABLESAMPLE, with unique names for a
-- relation read again in a nested query.
CREATE TABLE t377 (a int, b text, c numeric);
CREATE TABLE u377 (a int, d text);
CREATE VIEW v377_sub AS SELECT x.a, x.b FROM (SELECT a, b FROM t377 WHERE a > 1) x;
CREATE VIEW v377_sub2 AS SELECT * FROM (SELECT a, b FROM t377) AS x(p, q);
CREATE VIEW v377_func AS SELECT g FROM generate_series(1, 3) g;
CREATE VIEW v377_func2 AS SELECT * FROM generate_series(1, 3) AS g(i);
CREATE VIEW v377_func3 AS SELECT * FROM generate_series(1, 3);
CREATE VIEW v377_func4 AS SELECT * FROM unnest(ARRAY[1,2]) WITH ORDINALITY AS u(x, n);
CREATE VIEW v377_join AS SELECT t377.a, u377.d FROM t377 JOIN u377 ON t377.a = u377.a;
CREATE VIEW v377_using AS SELECT a, b, d FROM t377 JOIN u377 USING (a);
CREATE VIEW v377_using2 AS SELECT * FROM t377 LEFT JOIN u377 USING (a);
CREATE VIEW v377_natural AS SELECT * FROM t377 NATURAL JOIN u377;
CREATE VIEW v377_cross AS SELECT t377.a, u377.d FROM t377 CROSS JOIN u377;
CREATE VIEW v377_comma AS SELECT t377.a, u377.d FROM t377, u377 WHERE t377.a = u377.a;
CREATE VIEW v377_full AS SELECT t377.a, u377.d FROM t377 FULL JOIN u377 ON t377.a = u377.a AND u377.d <> 'x';
CREATE VIEW v377_nested AS SELECT t377.a FROM t377 JOIN (u377 JOIN t377 AS t3 ON u377.a = t3.a) ON t377.a = u377.a;
CREATE VIEW v377_nested2 AS SELECT j.a FROM (t377 JOIN u377 USING (a)) AS j;
CREATE VIEW v377_union AS SELECT a FROM t377 UNION SELECT a FROM u377;
CREATE VIEW v377_union3 AS SELECT a FROM t377 UNION ALL SELECT a FROM u377 EXCEPT SELECT 1 ORDER BY 1;
CREATE VIEW v377_union4 AS SELECT a FROM t377 UNION (SELECT a FROM u377 INTERSECT SELECT 2);
CREATE VIEW v377_don AS SELECT DISTINCT ON (a) a, b FROM t377 ORDER BY a, b DESC;
CREATE VIEW v377_dist AS SELECT DISTINCT a, b FROM t377;
CREATE VIEW v377_win AS SELECT a, row_number() OVER (PARTITION BY b ORDER BY a) AS rn, sum(a) OVER w AS s FROM t377 WINDOW w AS (ORDER BY a ROWS BETWEEN 1 PRECEDING AND CURRENT ROW);
CREATE VIEW v377_gs AS SELECT a, b, count(*) FROM t377 GROUP BY ROLLUP (a, b);
CREATE VIEW v377_gs2 AS SELECT a, b, count(*) FROM t377 GROUP BY GROUPING SETS ((a), (b), ());
CREATE VIEW v377_gs3 AS SELECT a, b, count(*) FROM t377 GROUP BY CUBE (a, b) HAVING count(*) > 1;
CREATE VIEW v377_cte AS WITH c AS (SELECT a FROM t377) SELECT a FROM c;
CREATE VIEW v377_cte2 AS WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT n FROM r;
CREATE VIEW v377_cte3 AS WITH c AS MATERIALIZED (SELECT a, b FROM t377), d AS NOT MATERIALIZED (SELECT a FROM u377) SELECT c.a, d.a AS da FROM c JOIN d ON c.a = d.a;
CREATE VIEW v377_lat AS SELECT t377.a, s.d FROM t377, LATERAL (SELECT d FROM u377 WHERE u377.a = t377.a) s;
CREATE VIEW v377_lat2 AS SELECT t377.a, g FROM t377 CROSS JOIN LATERAL generate_series(1, t377.a) g;
CREATE VIEW v377_values AS SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS v(n, s);
CREATE VIEW v377_values2 AS VALUES (1, 'a'), (2, 'b');
CREATE VIEW v377_sample AS SELECT a FROM t377 TABLESAMPLE system (10);
CREATE VIEW v377_sample2 AS SELECT a FROM t377 TABLESAMPLE bernoulli (5.5) REPEATABLE (42);
CREATE VIEW v377_self AS SELECT x.a, y.b FROM t377 x JOIN t377 y ON x.a = y.a;
CREATE VIEW v377_self2 AS SELECT t377.a FROM t377, (SELECT a FROM t377) s WHERE s.a = t377.a;
CREATE VIEW v377_in AS SELECT a FROM t377 WHERE a IN (SELECT a FROM u377);
CREATE VIEW v377_exists AS SELECT a FROM t377 WHERE EXISTS (SELECT 1 FROM u377 WHERE u377.a = t377.a);
CREATE VIEW v377_scalar AS SELECT a, (SELECT max(d) FROM u377) AS m FROM t377;
CREATE VIEW v377_limit AS SELECT a FROM t377 ORDER BY a LIMIT 3 OFFSET 1;
CREATE VIEW v377_ties AS SELECT a FROM t377 ORDER BY a FETCH FIRST 2 ROWS WITH TIES;
CREATE VIEW v377_lock AS SELECT a FROM t377 FOR UPDATE;
CREATE VIEW v377_having AS SELECT b, count(*) AS n FROM t377 GROUP BY b HAVING count(*) > 1;
CREATE VIEW v377_rowsfrom AS SELECT * FROM ROWS FROM (generate_series(1, 2), generate_series(1, 3)) AS r(x, y);
CREATE VIEW v377_viewview AS SELECT a FROM v377_sub WHERE a > 2;
CREATE VIEW v377_nofrom AS SELECT 1 AS one, 'x'::text AS t;
CREATE VIEW v377_subnoalias AS SELECT * FROM (SELECT a FROM t377);
SELECT c.relname, pg_get_viewdef(c.oid) FROM pg_class c
  WHERE c.relkind = 'v' AND c.relname LIKE 'v377%' ORDER BY 1;
SELECT c.relname, pg_get_viewdef(c.oid, true) FROM pg_class c
  WHERE c.relkind = 'v' AND c.relname LIKE 'v377%' ORDER BY 1;
SELECT c.relname, pg_get_viewdef(c.oid, 40) FROM pg_class c
  WHERE c.relkind = 'v' AND c.relname LIKE 'v377%' ORDER BY 1;
SELECT viewname, definition FROM pg_views WHERE viewname LIKE 'v377_s%' ORDER BY 1;
