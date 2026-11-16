-- PostgreSQL 18 EXPLAIN ANALYZE of data-modifying WITH queries: the write
-- runs once as the statement's "CTE name" InitPlan, its ModifyTable node
-- returns its RETURNING rows (none without RETURNING), and its scan reports
-- the rows it read and removed. Hash and CTE storage lines are the
-- allocator's own and are left out.
CREATE TABLE t561 (a int, b text);
INSERT INTO t561 SELECT g, 'v' || g FROM generate_series(1, 20) g;
ANALYZE t561;
CREATE FUNCTION analyzed561(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    CONTINUE WHEN line ~ '^\s*(Batches|Storage):';
    RETURN NEXT line;
  END LOOP;
END
$$;
SELECT analyzed561('WITH w AS (INSERT INTO t561 VALUES (100, ''z'') RETURNING *) SELECT * FROM w');
SELECT analyzed561('WITH w AS (DELETE FROM t561 WHERE a > 15 RETURNING a) SELECT count(*) FROM w');
SELECT analyzed561('WITH w AS (UPDATE t561 SET b = ''u'' WHERE a < 3 RETURNING a) SELECT * FROM w');
SELECT analyzed561('WITH w AS (UPDATE t561 SET b = ''x'' WHERE a < 3 RETURNING a) SELECT * FROM w, t561 WHERE w.a = t561.a');
SELECT analyzed561('WITH w AS (DELETE FROM t561 WHERE a = 5) SELECT 1');
SELECT a, b FROM t561 ORDER BY a;
