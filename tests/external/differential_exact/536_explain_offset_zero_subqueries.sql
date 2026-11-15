-- PostgreSQL 18 limit_needed: a constant OFFSET 0 or LIMIT NULL needs no
-- Limit node, but a subquery's OFFSET 0 still keeps it from being pulled up.
-- A join input's Subquery Scan without a filter of its own is trivial and
-- set_plan_references removes it (trivial_subqueryscan), so the join's
-- conditions print the columns the subquery's own plan outputs.
-- Materialize memory lines are the allocator's own and are left out.
CREATE TABLE o536 (a int, b text);
CREATE TABLE p536 (a int, b text);
INSERT INTO o536 SELECT g, 'x' || g FROM generate_series(1, 20) g;
INSERT INTO p536 SELECT g, 'y' || g FROM generate_series(1, 20) g;
ANALYZE o536;
ANALYZE p536;
CREATE FUNCTION analyzed536(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    CONTINUE WHEN line ~ '^\s*(Buckets|Batches|Storage):';
    RETURN NEXT line;
  END LOOP;
END
$$;
EXPLAIN (COSTS OFF) SELECT * FROM o536 OFFSET 0;
EXPLAIN (COSTS OFF) SELECT * FROM o536 LIMIT NULL;
EXPLAIN (COSTS OFF) SELECT * FROM o536 OFFSET NULL;
EXPLAIN (COSTS OFF) SELECT * FROM o536 LIMIT ALL OFFSET 0;
EXPLAIN (COSTS OFF) SELECT * FROM o536 OFFSET 1;
EXPLAIN (COSTS OFF) SELECT * FROM o536 UNION ALL SELECT * FROM p536 OFFSET 0;
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT * FROM o536 OFFSET 0) x, (SELECT * FROM p536 OFFSET 0) y WHERE x.a < y.a;
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT * FROM o536 t OFFSET 0) x, (SELECT b, a FROM p536 OFFSET 0) y WHERE x.a < y.a;
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT * FROM o536 OFFSET 0) x, (SELECT * FROM p536 OFFSET 0) y WHERE x.a < y.a AND x.b <> y.b;
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT * FROM o536 OFFSET 0) x LEFT JOIN p536 ON x.a = p536.a;
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT * FROM o536 OFFSET 0) x WHERE x.a > 3;
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT a FROM o536 OFFSET 0) x;
EXPLAIN (COSTS OFF) SELECT x.a FROM (SELECT * FROM o536 LIMIT 5) x;
SELECT analyzed536('SELECT * FROM (SELECT * FROM o536 OFFSET 0) x, (SELECT * FROM p536 OFFSET 0) y WHERE x.a < y.a');
SELECT analyzed536('SELECT * FROM o536 OFFSET 0');
SELECT count(*) FROM (SELECT * FROM o536 OFFSET 0) x, (SELECT * FROM p536 OFFSET 0) y WHERE x.a < y.a AND x.b <> y.b;
SELECT * FROM o536 ORDER BY a OFFSET 0 LIMIT 2;
SELECT * FROM o536 ORDER BY a LIMIT NULL OFFSET 18;
