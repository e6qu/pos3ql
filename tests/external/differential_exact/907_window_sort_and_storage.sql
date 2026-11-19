-- PostgreSQL 18 EXPLAIN ANALYZE of windows: the Sort under each WindowAgg
-- holds the window input target (make_window_input_target: the window keys,
-- then the columns the rest of the target list reads, then the functions of
-- the windows below) and reports its memory or its temporary file like any
-- tuplesort; the WindowAgg's "Maximum Storage" is its partition tuplestore,
-- which trims the rows behind each function's mark and read pointers
-- (tuplestore_trim) and the frame tail of a RANGE frame, and a partition
-- outgrowing work_mem is written to a file whole ("Storage: Disk"). Frames
-- with offsets, exclusion or GROUPS and windows beyond row_number, rank,
-- dense_rank and plain aggregates fail with 0A000.
CREATE TABLE t907 (a int, b int, c text, d numeric, e int);
INSERT INTO t907 SELECT g % 5, (g * 37) % 101, CASE WHEN g % 9 = 0 THEN NULL ELSE repeat(chr(97 + g % 26), g % 40) END, g / 7.0, CASE WHEN g % 13 = 0 THEN NULL ELSE g % 4 END FROM generate_series(1, 1500) g;
ANALYZE t907;
CREATE FUNCTION windowed907(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    CONTINUE WHEN line !~ '(Storage|Sort Method|Window:|Sort Key)';
    RETURN NEXT line;
  END LOOP;
END
$$;

SET work_mem = '4MB';
SELECT windowed907($q$SELECT *, count(e) OVER (partition by a, e order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT *, count(*) OVER (partition by c order by d desc), dense_rank() OVER (partition by c order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT b, rank() OVER (partition by a, e order by c) FROM t907$q$);
SELECT windowed907($q$SELECT a, e, c, count(e) OVER (order by b, a) FROM t907$q$);
SELECT windowed907($q$SELECT *, dense_rank() OVER (partition by c) FROM t907$q$);
SELECT windowed907($q$SELECT b, row_number() OVER (order by b) FROM t907$q$);
SELECT windowed907($q$SELECT *, row_number() OVER (partition by e order by d desc), avg(e) OVER (partition by e order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, count(e) OVER (partition by a order by e nulls first) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, row_number() OVER (order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, count(e) OVER (order by e nulls first) FROM t907$q$);
SELECT windowed907($q$SELECT a, count(*) OVER (partition by c order by c) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, rank() OVER (partition by a, e) FROM t907$q$);
SELECT windowed907($q$SELECT a, rank() OVER (order by e nulls first) FROM t907$q$);
SELECT windowed907($q$SELECT a, e, c, count(*) OVER (order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT b, min(d) OVER (order by b, a), count(e) OVER (order by b, a) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, sum(b) OVER (order by c), max(c) OVER (order by c) FROM t907$q$);
SELECT windowed907($q$SELECT b, min(d) OVER (order by b) FROM t907$q$);
SELECT windowed907($q$SELECT a, e, c, rank() OVER (order by d desc), row_number() OVER (order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT *, count(e) OVER (partition by a order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT a, e, c, dense_rank() OVER (partition by a, e order by e nulls first) FROM t907$q$);
SELECT windowed907($q$SELECT a, min(d) OVER (partition by a, e order by b) FROM t907$q$);
SELECT windowed907($q$SELECT b, sum(b) OVER (order by b) FROM t907$q$);
SELECT windowed907($q$SELECT b, rank() OVER (order by b) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, sum(b) OVER (order by b, a) FROM t907$q$);
SELECT windowed907($q$SELECT a, min(d) OVER () FROM t907$q$);
SELECT windowed907($q$SELECT c, d, count(*) OVER (order by b, a) FROM t907$q$);
SELECT windowed907($q$SELECT a, e, c, avg(e) OVER (partition by a order by b, a) FROM t907$q$);
SELECT windowed907($q$SELECT b, dense_rank() OVER (partition by a order by e nulls first), min(d) OVER (partition by a order by e nulls first) FROM t907$q$);
SELECT windowed907($q$SELECT a, row_number() OVER (ORDER BY b), rank() OVER (PARTITION BY a ORDER BY b) FROM t907$q$);
SELECT windowed907($q$SELECT b, a + 1, rank() OVER (ORDER BY a + 1) FROM t907$q$);
SELECT windowed907($q$SELECT a, sum(b) OVER w, count(*) OVER w FROM t907 WINDOW w AS (PARTITION BY a ORDER BY b)$q$);
SELECT windowed907($q$SELECT a, sum(b) OVER (PARTITION BY a) FROM t907 ORDER BY b DESC LIMIT 4$q$);
SELECT windowed907($q$SELECT a, sum(b) OVER (PARTITION BY a) FROM t907 ORDER BY b DESC$q$);
SELECT windowed907($q$SELECT c, count(*) OVER (ORDER BY c) FROM t907 ORDER BY c DESC LIMIT 700$q$);
SET work_mem = '128kB';
SELECT windowed907($q$SELECT *, count(e) OVER (partition by a, e order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT *, count(*) OVER (partition by c order by d desc), dense_rank() OVER (partition by c order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT b, rank() OVER (partition by a, e order by c) FROM t907$q$);
SELECT windowed907($q$SELECT a, e, c, count(e) OVER (order by b, a) FROM t907$q$);
SELECT windowed907($q$SELECT *, dense_rank() OVER (partition by c) FROM t907$q$);
SELECT windowed907($q$SELECT b, row_number() OVER (order by b) FROM t907$q$);
SELECT windowed907($q$SELECT *, row_number() OVER (partition by e order by d desc), avg(e) OVER (partition by e order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, count(e) OVER (partition by a order by e nulls first) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, row_number() OVER (order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, count(e) OVER (order by e nulls first) FROM t907$q$);
SELECT windowed907($q$SELECT a, count(*) OVER (partition by c order by c) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, rank() OVER (partition by a, e) FROM t907$q$);
SELECT windowed907($q$SELECT a, rank() OVER (order by e nulls first) FROM t907$q$);
SELECT windowed907($q$SELECT a, e, c, count(*) OVER (order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT b, min(d) OVER (order by b, a), count(e) OVER (order by b, a) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, sum(b) OVER (order by c), max(c) OVER (order by c) FROM t907$q$);
SELECT windowed907($q$SELECT b, min(d) OVER (order by b) FROM t907$q$);
SELECT windowed907($q$SELECT a, e, c, rank() OVER (order by d desc), row_number() OVER (order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT *, count(e) OVER (partition by a order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT a, e, c, dense_rank() OVER (partition by a, e order by e nulls first) FROM t907$q$);
SELECT windowed907($q$SELECT a, min(d) OVER (partition by a, e order by b) FROM t907$q$);
SELECT windowed907($q$SELECT b, sum(b) OVER (order by b) FROM t907$q$);
SELECT windowed907($q$SELECT b, rank() OVER (order by b) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, sum(b) OVER (order by b, a) FROM t907$q$);
SELECT windowed907($q$SELECT a, min(d) OVER () FROM t907$q$);
SELECT windowed907($q$SELECT c, d, count(*) OVER (order by b, a) FROM t907$q$);
SELECT windowed907($q$SELECT a, e, c, avg(e) OVER (partition by a order by b, a) FROM t907$q$);
SELECT windowed907($q$SELECT b, dense_rank() OVER (partition by a order by e nulls first), min(d) OVER (partition by a order by e nulls first) FROM t907$q$);
SELECT windowed907($q$SELECT a, row_number() OVER (ORDER BY b), rank() OVER (PARTITION BY a ORDER BY b) FROM t907$q$);
SELECT windowed907($q$SELECT b, a + 1, rank() OVER (ORDER BY a + 1) FROM t907$q$);
SELECT windowed907($q$SELECT a, sum(b) OVER w, count(*) OVER w FROM t907 WINDOW w AS (PARTITION BY a ORDER BY b)$q$);
SELECT windowed907($q$SELECT a, sum(b) OVER (PARTITION BY a) FROM t907 ORDER BY b DESC LIMIT 4$q$);
SELECT windowed907($q$SELECT a, sum(b) OVER (PARTITION BY a) FROM t907 ORDER BY b DESC$q$);
SELECT windowed907($q$SELECT c, count(*) OVER (ORDER BY c) FROM t907 ORDER BY c DESC LIMIT 700$q$);
SET work_mem = '64kB';
SELECT windowed907($q$SELECT *, count(e) OVER (partition by a, e order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT *, count(*) OVER (partition by c order by d desc), dense_rank() OVER (partition by c order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT b, rank() OVER (partition by a, e order by c) FROM t907$q$);
SELECT windowed907($q$SELECT a, e, c, count(e) OVER (order by b, a) FROM t907$q$);
SELECT windowed907($q$SELECT *, dense_rank() OVER (partition by c) FROM t907$q$);
SELECT windowed907($q$SELECT b, row_number() OVER (order by b) FROM t907$q$);
SELECT windowed907($q$SELECT *, row_number() OVER (partition by e order by d desc), avg(e) OVER (partition by e order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, count(e) OVER (partition by a order by e nulls first) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, row_number() OVER (order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, count(e) OVER (order by e nulls first) FROM t907$q$);
SELECT windowed907($q$SELECT a, count(*) OVER (partition by c order by c) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, rank() OVER (partition by a, e) FROM t907$q$);
SELECT windowed907($q$SELECT a, rank() OVER (order by e nulls first) FROM t907$q$);
SELECT windowed907($q$SELECT a, e, c, count(*) OVER (order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT b, min(d) OVER (order by b, a), count(e) OVER (order by b, a) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, sum(b) OVER (order by c), max(c) OVER (order by c) FROM t907$q$);
SELECT windowed907($q$SELECT b, min(d) OVER (order by b) FROM t907$q$);
SELECT windowed907($q$SELECT a, e, c, rank() OVER (order by d desc), row_number() OVER (order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT *, count(e) OVER (partition by a order by d desc) FROM t907$q$);
SELECT windowed907($q$SELECT a, e, c, dense_rank() OVER (partition by a, e order by e nulls first) FROM t907$q$);
SELECT windowed907($q$SELECT a, min(d) OVER (partition by a, e order by b) FROM t907$q$);
SELECT windowed907($q$SELECT b, sum(b) OVER (order by b) FROM t907$q$);
SELECT windowed907($q$SELECT b, rank() OVER (order by b) FROM t907$q$);
SELECT windowed907($q$SELECT c, d, sum(b) OVER (order by b, a) FROM t907$q$);
SELECT windowed907($q$SELECT a, min(d) OVER () FROM t907$q$);
SELECT windowed907($q$SELECT c, d, count(*) OVER (order by b, a) FROM t907$q$);
SELECT windowed907($q$SELECT a, e, c, avg(e) OVER (partition by a order by b, a) FROM t907$q$);
SELECT windowed907($q$SELECT b, dense_rank() OVER (partition by a order by e nulls first), min(d) OVER (partition by a order by e nulls first) FROM t907$q$);
SELECT windowed907($q$SELECT a, row_number() OVER (ORDER BY b), rank() OVER (PARTITION BY a ORDER BY b) FROM t907$q$);
SELECT windowed907($q$SELECT b, a + 1, rank() OVER (ORDER BY a + 1) FROM t907$q$);
SELECT windowed907($q$SELECT a, sum(b) OVER w, count(*) OVER w FROM t907 WINDOW w AS (PARTITION BY a ORDER BY b)$q$);
SELECT windowed907($q$SELECT a, sum(b) OVER (PARTITION BY a) FROM t907 ORDER BY b DESC LIMIT 4$q$);
SELECT windowed907($q$SELECT a, sum(b) OVER (PARTITION BY a) FROM t907 ORDER BY b DESC$q$);
SELECT windowed907($q$SELECT c, count(*) OVER (ORDER BY c) FROM t907 ORDER BY c DESC LIMIT 700$q$);
