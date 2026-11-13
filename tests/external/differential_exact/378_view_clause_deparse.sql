-- Deparsed views over USING and FULL joins, sublinks under operators and
-- NOT, searched CASE blocks, aggregates with DISTINCT, ORDER BY and FILTER,
-- named and copied windows with frames, schema-qualified and ONLY tables,
-- column aliases, `*` over several items, grouping by expressions and output
-- names, LIMIT ALL, row locks and VALUES lists of mixed types. A view may
-- group by an output column's name, as PostgreSQL allows.
CREATE TABLE t378 (a int, b text, c numeric);
CREATE TABLE u378 (a int, d text);
CREATE SCHEMA s378;
CREATE TABLE s378.w378 (a int, e text);
CREATE VIEW v378_fullusing AS SELECT a, b, d FROM t378 FULL JOIN u378 USING (a);
CREATE VIEW v378_rightusing AS SELECT a, d FROM t378 RIGHT JOIN u378 USING (a);
CREATE VIEW v378_usingalias AS SELECT x.a, t378.b FROM t378 JOIN u378 USING (a) AS x;
CREATE VIEW v378_notexists AS SELECT a FROM t378 WHERE NOT EXISTS (SELECT 1 FROM u378 WHERE u378.a = t378.a);
CREATE VIEW v378_notin AS SELECT a FROM t378 WHERE a NOT IN (SELECT a FROM u378) AND b > 'x';
CREATE VIEW v378_any AS SELECT a FROM t378 WHERE a > ANY (SELECT a FROM u378);
CREATE VIEW v378_scalarop AS SELECT a + (SELECT max(a) FROM u378) AS s FROM t378;
CREATE VIEW v378_array AS SELECT ARRAY(SELECT a FROM u378) AS arr;
CREATE VIEW v378_case AS SELECT CASE WHEN a > 1 THEN 'big' WHEN a < 0 THEN 'neg' END AS k, CASE WHEN a > 1 THEN 1 ELSE 0 END AS z FROM t378;
CREATE VIEW v378_agg AS SELECT count(DISTINCT a) AS n, string_agg(b, ',' ORDER BY b DESC) AS s, sum(a) FILTER (WHERE a > 0) AS p FROM t378;
CREATE VIEW v378_win2 AS SELECT a, sum(a) OVER (w ORDER BY a) AS s, rank() OVER w AS r, avg(a) OVER (ORDER BY a ROWS 2 PRECEDING) AS m FROM t378 WINDOW w AS (PARTITION BY b);
CREATE VIEW v378_win3 AS SELECT a, first_value(a) OVER (PARTITION BY b ORDER BY a DESC NULLS LAST RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE TIES) AS f FROM t378;
CREATE VIEW v378_schema AS SELECT w378.a, w378.e FROM s378.w378;
CREATE VIEW v378_only AS SELECT a FROM ONLY t378;
CREATE VIEW v378_selfalias AS SELECT t378.a FROM t378 t378;
CREATE VIEW v378_colalias AS SELECT x.p FROM t378 AS x(p, q);
CREATE VIEW v378_star2 AS SELECT * FROM t378, u378;
CREATE VIEW v378_groupexpr AS SELECT lower(b) AS lb, a + 1 AS a1, count(*) FROM t378 GROUP BY lower(b), a + 1 ORDER BY 1, 2;
CREATE VIEW v378_groupalias AS SELECT b AS bb, count(*) FROM t378 GROUP BY bb ORDER BY bb;
CREATE VIEW v378_mixed AS SELECT a, b, count(*) FROM t378 GROUP BY a, ROLLUP (b, c), CUBE ((a, b));
CREATE VIEW v378_gdistinct AS SELECT a, count(*) FROM t378 GROUP BY DISTINCT ROLLUP (a), ROLLUP (a);
CREATE VIEW v378_subsub AS SELECT * FROM (SELECT * FROM (SELECT a FROM t378) x) y;
CREATE VIEW v378_join3 AS SELECT t378.a FROM t378 LEFT JOIN u378 ON t378.a = u378.a LEFT JOIN s378.w378 ON w378.a = u378.a;
CREATE VIEW v378_unionwith AS (SELECT a FROM t378 ORDER BY a LIMIT 1) UNION ALL SELECT a FROM u378;
CREATE VIEW v378_valuesjoin AS SELECT t378.a, v.x FROM t378 JOIN (VALUES (1), (2)) v(x) ON v.x = t378.a;
CREATE VIEW v378_valuesnoalias AS SELECT * FROM (VALUES (1, 2.5), (2, 3)) v;
CREATE VIEW v378_lockmulti AS SELECT t378.a FROM t378, u378 FOR SHARE OF u378 NOWAIT;
CREATE VIEW v378_cast AS SELECT a::text AS at, CAST(c AS int) AS ci, b || 'x' AS bx FROM t378 WHERE c::int > 3;
CREATE VIEW v378_inlist AS SELECT a FROM t378 WHERE a IN (1, 2, 3) AND b LIKE 'x%' AND c IS NOT NULL;
CREATE VIEW v378_distinctlimit AS SELECT DISTINCT a FROM t378 ORDER BY a DESC LIMIT ALL;
SELECT c.relname, pg_get_viewdef(c.oid) FROM pg_class c
  WHERE c.relkind = 'v' AND c.relname LIKE 'v378%' ORDER BY 1;
SELECT c.relname, pg_get_viewdef(c.oid, true) FROM pg_class c
  WHERE c.relkind = 'v' AND c.relname LIKE 'v378%' ORDER BY 1;
SELECT c.relname, pg_get_viewdef(c.oid, 40) FROM pg_class c
  WHERE c.relkind = 'v' AND c.relname LIKE 'v378%' ORDER BY 1;
SELECT bb, count FROM v378_groupalias;
