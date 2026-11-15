-- PostgreSQL 18 SS_process_ctes at every query level: a WITH list inside a
-- derived table or a subquery keeps a query run once (MATERIALIZED, or
-- read twice) as an InitPlan "CTE name" of that query's top node and
-- inlines the rest; a subquery reading only its own WITH queries is
-- uncorrelated and runs once as an InitPlan.
CREATE TABLE t562 (a int, b text);
INSERT INTO t562 SELECT g, 'v' || g FROM generate_series(1, 30) g;
ANALYZE t562;
EXPLAIN (COSTS OFF) SELECT * FROM (WITH c AS MATERIALIZED (SELECT * FROM t562) SELECT * FROM c) s;
EXPLAIN (COSTS OFF) SELECT * FROM (WITH c AS (SELECT * FROM t562) SELECT * FROM c) s;
EXPLAIN (COSTS OFF) SELECT * FROM (WITH c AS (SELECT * FROM t562) SELECT * FROM c x, c y WHERE x.a = y.a) s;
EXPLAIN (COSTS OFF) SELECT (WITH c AS MATERIALIZED (SELECT max(a) m FROM t562) SELECT m FROM c) FROM t562 LIMIT 1;
SELECT (WITH c AS MATERIALIZED (SELECT max(a) m FROM t562) SELECT m FROM c) FROM t562 LIMIT 2;
SELECT a, (WITH c AS (SELECT t562.a + 1 AS v) SELECT v FROM c) FROM t562 WHERE a < 3 ORDER BY a;
SELECT * FROM (WITH c AS MATERIALIZED (SELECT * FROM t562 WHERE a < 3) SELECT * FROM c) s ORDER BY a;
SELECT a FROM t562 WHERE a IN (WITH c AS MATERIALIZED (SELECT a FROM t562 WHERE a < 3) SELECT a FROM c) ORDER BY a;
EXPLAIN (COSTS OFF) SELECT a, (SELECT t562.a + count(*) FROM t562 x) FROM t562 WHERE a < 3;
SELECT a, (SELECT t562.a + count(*) FROM t562 x) FROM t562 WHERE a < 3 ORDER BY a;
