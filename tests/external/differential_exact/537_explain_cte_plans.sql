-- PostgreSQL 18 SS_process_ctes: a WITH query is inlined only when it is a
-- non-recursive SELECT without volatile functions that is NOT MATERIALIZED
-- or, by default, read once. Every other referenced query runs once as an
-- InitPlan "CTE name" of the statement's top node, read by CTE Scans that
-- keep their own rows (no Materialize above them), and takes a subplan
-- number before the query's own subqueries. A recursive query is a
-- Recursive Union whose recursive term scans the working table. A relation
-- read twice through inlined queries gets a unique range-table name.
-- Materialize and CTE memory lines are the allocator's own and are left out.
CREATE TABLE t537 (a int, b text);
INSERT INTO t537 SELECT g, 'v' || g FROM generate_series(1, 30) g;
ANALYZE t537;
CREATE FUNCTION analyzed537(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    CONTINUE WHEN line ~ '^\s*(Buckets|Batches|Storage):';
    RETURN NEXT line;
  END LOOP;
END
$$;
EXPLAIN (COSTS OFF) WITH c AS (SELECT * FROM t537) SELECT * FROM c;
EXPLAIN (COSTS OFF) WITH c AS (SELECT * FROM t537) SELECT * FROM c a, c b WHERE a.a = b.a;
EXPLAIN (COSTS OFF) WITH c AS (SELECT * FROM t537) SELECT * FROM c a, c b WHERE a.a < b.a;
EXPLAIN (COSTS OFF) WITH c AS MATERIALIZED (SELECT * FROM t537) SELECT * FROM c;
EXPLAIN (COSTS OFF) WITH c AS MATERIALIZED (SELECT * FROM t537 WHERE a > 5) SELECT * FROM c WHERE b <> 'x';
EXPLAIN (COSTS OFF) WITH c AS MATERIALIZED (SELECT * FROM t537) SELECT * FROM c a, c b WHERE a.a < b.a;
EXPLAIN (COSTS OFF) WITH c AS NOT MATERIALIZED (SELECT * FROM t537) SELECT * FROM c a, c b WHERE a.a < b.a;
EXPLAIN (COSTS OFF) WITH c AS (SELECT a, random() r FROM t537) SELECT * FROM c;
EXPLAIN (COSTS OFF) WITH c AS MATERIALIZED (SELECT * FROM t537), d AS (SELECT 1) SELECT * FROM c;
EXPLAIN (COSTS OFF) WITH c AS MATERIALIZED (SELECT * FROM t537), d AS MATERIALIZED (SELECT * FROM c WHERE a < 10) SELECT * FROM d;
EXPLAIN (COSTS OFF) WITH c AS (SELECT a FROM t537 UNION SELECT a + 1 FROM t537) SELECT * FROM c x, c y WHERE x.a = y.a;
EXPLAIN (COSTS OFF) WITH c AS MATERIALIZED (SELECT a, count(*) FROM t537 GROUP BY a) SELECT * FROM c ORDER BY a;
EXPLAIN (COSTS OFF) WITH c AS MATERIALIZED (SELECT * FROM t537) SELECT (SELECT max(a) FROM t537), * FROM c WHERE a > (SELECT 1);
EXPLAIN (COSTS OFF, FORMAT JSON) WITH c AS MATERIALIZED (SELECT * FROM t537) SELECT * FROM c x;
EXPLAIN (COSTS OFF) WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT * FROM r;
EXPLAIN (COSTS OFF) WITH RECURSIVE r(n) AS (SELECT 1 UNION SELECT n + 1 FROM r WHERE n < 5) SELECT * FROM r;
EXPLAIN (COSTS OFF) WITH w AS (INSERT INTO t537 VALUES (100, 'z') RETURNING *) SELECT * FROM w;
EXPLAIN (COSTS OFF) WITH w AS (DELETE FROM t537 WHERE a = 100 RETURNING a) SELECT count(*) FROM w;
EXPLAIN (COSTS OFF) WITH w AS (UPDATE t537 SET b = 'u' WHERE a = 1 RETURNING a) SELECT * FROM w, t537 WHERE w.a = t537.a;
SELECT analyzed537('WITH c AS MATERIALIZED (SELECT * FROM t537) SELECT * FROM c a, c b WHERE a.a < b.a');
SELECT analyzed537('WITH c AS MATERIALIZED (SELECT * FROM t537 WHERE a > 20) SELECT * FROM c a, c b WHERE a.a = b.a');
SELECT analyzed537('WITH c AS (SELECT a, random() r FROM t537) SELECT a FROM c WHERE a < 4');
SELECT analyzed537('WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT * FROM r');
WITH c AS MATERIALIZED (SELECT * FROM t537 WHERE a > 27) SELECT * FROM c a, c b WHERE a.a = b.a ORDER BY 1;
WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT * FROM r;
