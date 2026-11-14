-- PostgreSQL 18 plans one WindowAgg per window clause, in the order
-- select_active_windows sorts them, each over a sort of its input unless
-- the rows already follow its keys (incrementally when they follow a
-- prefix), and names unnamed windows w1, w2, ... (name_active_windows).
-- show_window_def prints the definition with keys but no directions, and
-- the frame ROWS UNBOUNDED PRECEDING that optimize_window_clauses gives
-- windows whose functions do not depend on the frame.
CREATE TABLE t472 (a int, b int);
INSERT INTO t472 SELECT g % 10, g FROM generate_series(1, 100) g;
CREATE TABLE u472 (a int, c text);
INSERT INTO u472 SELECT g, 'x' || g FROM generate_series(1, 20) g;
ANALYZE t472;
ANALYZE u472;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER () FROM t472;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER (PARTITION BY a) FROM t472;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER (ORDER BY b DESC) FROM t472;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER (PARTITION BY a ORDER BY b NULLS FIRST) FROM t472;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER (PARTITION BY a ORDER BY b) FROM t472 ORDER BY a;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER (PARTITION BY a ORDER BY b) FROM t472 ORDER BY a, b;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER (PARTITION BY a) FROM t472 ORDER BY a, b;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER (PARTITION BY a) FROM t472 ORDER BY b;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER w FROM t472 WINDOW w AS (PARTITION BY a);
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER w, count(*) OVER w FROM t472 WINDOW w AS (ORDER BY a);
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER (ORDER BY b ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t472;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER (ORDER BY b ROWS UNBOUNDED PRECEDING) FROM t472;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER (ORDER BY b GROUPS BETWEEN CURRENT ROW AND 2 FOLLOWING EXCLUDE TIES) FROM t472;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER (PARTITION BY a % 3 ORDER BY b + 1) FROM t472;
EXPLAIN (COSTS OFF) SELECT a, row_number() OVER (ORDER BY b), rank() OVER (PARTITION BY a ORDER BY b) FROM t472;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER (PARTITION BY a ORDER BY b), count(*) OVER (PARTITION BY a) FROM t472;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER (ORDER BY a), count(*) OVER (ORDER BY a, b DESC) FROM t472;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER (PARTITION BY a ORDER BY b), avg(b) OVER (PARTITION BY a ORDER BY b), max(b) OVER w FROM t472 WINDOW w AS (PARTITION BY a ORDER BY b);
EXPLAIN (COSTS OFF) SELECT t472.a, sum(u472.a) OVER (PARTITION BY t472.a) FROM t472 JOIN u472 ON t472.b = u472.a;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER (PARTITION BY a) FROM t472 WHERE b > 10 LIMIT 3;
EXPLAIN (COSTS OFF) SELECT a, ntile(3) OVER (ORDER BY b ROWS BETWEEN 2 PRECEDING AND 2 FOLLOWING) FROM t472;
EXPLAIN (COSTS OFF) SELECT a, sum(b) OVER () FROM t472 WHERE false;
EXPLAIN (COSTS OFF, FORMAT JSON) SELECT a, sum(b) OVER (PARTITION BY a) FROM t472 ORDER BY a, b;
CREATE FUNCTION analyzed472(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    -- Memory figures are each server's own.
    RETURN NEXT regexp_replace(line, '(Memory|Maximum Storage): \d+kB', '\1: NkB');
  END LOOP;
END
$$;
SELECT analyzed472('SELECT a, sum(b) OVER (PARTITION BY a ORDER BY b) FROM t472');
SELECT analyzed472('SELECT a, sum(b) OVER () FROM t472 WHERE a < 3');
SELECT analyzed472('SELECT a, sum(b) OVER (PARTITION BY a) FROM t472 ORDER BY b DESC LIMIT 4');
SELECT analyzed472('SELECT a, row_number() OVER (ORDER BY b), rank() OVER (PARTITION BY a ORDER BY b) FROM t472');
SELECT a, row_number() OVER (ORDER BY b), rank() OVER (PARTITION BY a ORDER BY b) FROM t472 WHERE b < 13;
