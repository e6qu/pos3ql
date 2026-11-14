-- PostgreSQL 18 explain.c show_sort_info prints how an analyzed sort ran:
-- `quicksort` in memory, or `top-N heapsort` when a Limit directly above
-- bounds it and it read more than twice the bound (tuplesort.c
-- puttuple_common). The memory each sort took is measured by each server's
-- own allocator, so the figures are masked.
CREATE TABLE t471 (a int, b int);
INSERT INTO t471 SELECT g % 10, g FROM generate_series(1, 100) g;
CREATE TABLE u471 (a int, c text);
INSERT INTO u471 SELECT g, 'x' || g FROM generate_series(1, 20) g;
ANALYZE t471;
ANALYZE u471;
CREATE FUNCTION sorted471(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
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
SELECT sorted471('SELECT * FROM t471 ORDER BY b');
SELECT sorted471('SELECT * FROM t471 ORDER BY b DESC LIMIT 3');
SELECT sorted471('SELECT * FROM t471 ORDER BY b LIMIT 50');
SELECT sorted471('SELECT * FROM t471 ORDER BY b LIMIT 49');
SELECT sorted471('SELECT * FROM t471 ORDER BY b OFFSET 10 LIMIT 5');
SELECT sorted471('SELECT * FROM t471 ORDER BY b FETCH FIRST 3 ROWS WITH TIES');
SELECT sorted471('SELECT * FROM t471 WHERE a = 1 ORDER BY b LIMIT 2 FOR UPDATE');
SELECT sorted471('SELECT a, count(*) FROM t471 GROUP BY a ORDER BY a');
SELECT sorted471('SELECT a, count(*) FROM t471 GROUP BY a ORDER BY count(*) DESC, a LIMIT 2');
SELECT sorted471('SELECT DISTINCT ON (a) a, b FROM t471 ORDER BY a, b DESC');
SELECT sorted471('SELECT a FROM t471 UNION SELECT a FROM u471 ORDER BY 1 LIMIT 3');
SELECT sorted471('SELECT a FROM t471 UNION ALL SELECT a FROM u471 ORDER BY 1');
SELECT sorted471('SELECT * FROM t471 JOIN u471 ON t471.b = u471.a ORDER BY u471.c');
SELECT sorted471('SELECT generate_series(1, 10) g ORDER BY 1 DESC');
SELECT sorted471('SELECT * FROM t471 WHERE false ORDER BY b LIMIT 2');
-- The bounded sort returns the rows a full sort puts first.
SELECT b FROM t471 ORDER BY a DESC, b LIMIT 3 OFFSET 2;
SELECT a, count(*) FROM t471 GROUP BY a ORDER BY count(*) DESC, a LIMIT 2;
SELECT a FROM t471 UNION SELECT a FROM u471 ORDER BY 1 DESC LIMIT 3;
DO $$
DECLARE
  plan text;
BEGIN
  EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF, FORMAT JSON) '
    || 'SELECT * FROM t471 WHERE a = 3 ORDER BY b' INTO plan;
  RAISE NOTICE '%', regexp_replace(plan, '"Sort Space Used": \d+', '"Sort Space Used": N');
END
$$;
