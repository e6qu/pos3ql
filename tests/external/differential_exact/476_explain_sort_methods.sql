-- PostgreSQL 18 explain.c show_sort_info prints how an analyzed sort ran:
-- `quicksort` in memory, or `top-N heapsort` when a Limit directly above
-- bounds it and it read more than twice the bound (tuplesort.c
-- puttuple_common). The memory each sort took is measured by each server's
-- own allocator, so the figures are masked.
CREATE TABLE t476 (a int, b int);
INSERT INTO t476 SELECT g % 10, g FROM generate_series(1, 100) g;
CREATE TABLE u476 (a int, c text);
INSERT INTO u476 SELECT g, 'x' || g FROM generate_series(1, 20) g;
ANALYZE t476;
ANALYZE u476;
CREATE FUNCTION sorted476(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    -- HashAggregate's batch line is not shown by pos3ql yet, and its hash
    -- tables size their buckets differently.
    CONTINUE WHEN line ~ '^\s*(Batches|Buckets):';
    RETURN NEXT regexp_replace(line, '(Memory|Memory Usage): \d+kB', '\1: NkB');
  END LOOP;
END
$$;
SELECT sorted476('SELECT * FROM t476 ORDER BY b');
SELECT sorted476('SELECT * FROM t476 ORDER BY b DESC LIMIT 3');
SELECT sorted476('SELECT * FROM t476 ORDER BY b LIMIT 50');
SELECT sorted476('SELECT * FROM t476 ORDER BY b LIMIT 49');
SELECT sorted476('SELECT * FROM t476 ORDER BY b OFFSET 10 LIMIT 5');
SELECT sorted476('SELECT * FROM t476 ORDER BY b FETCH FIRST 3 ROWS WITH TIES');
SELECT sorted476('SELECT * FROM t476 WHERE a = 1 ORDER BY b LIMIT 2 FOR UPDATE');
SELECT sorted476('SELECT a, count(*) FROM t476 GROUP BY a ORDER BY a');
SELECT sorted476('SELECT a, count(*) FROM t476 GROUP BY a ORDER BY count(*) DESC, a LIMIT 2');
SELECT sorted476('SELECT DISTINCT ON (a) a, b FROM t476 ORDER BY a, b DESC');
SELECT sorted476('SELECT a FROM t476 UNION SELECT a FROM u476 ORDER BY 1 LIMIT 3');
SELECT sorted476('SELECT a FROM t476 UNION ALL SELECT a FROM u476 ORDER BY 1');
SELECT sorted476('SELECT * FROM t476 JOIN u476 ON t476.b = u476.a ORDER BY u476.c');
SELECT sorted476('SELECT generate_series(1, 10) g ORDER BY 1 DESC');
SELECT sorted476('SELECT * FROM t476 WHERE false ORDER BY b LIMIT 2');
-- The bounded sort returns the rows a full sort puts first.
SELECT b FROM t476 ORDER BY a DESC, b LIMIT 3 OFFSET 2;
SELECT a, count(*) FROM t476 GROUP BY a ORDER BY count(*) DESC, a LIMIT 2;
SELECT a FROM t476 UNION SELECT a FROM u476 ORDER BY 1 DESC LIMIT 3;
DO $$
DECLARE
  plan text;
BEGIN
  EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF, FORMAT JSON) '
    || 'SELECT * FROM t476 WHERE a = 3 ORDER BY b' INTO plan;
  RAISE NOTICE '%', regexp_replace(plan, '"Sort Space Used": \d+', '"Sort Space Used": N');
END
$$;
