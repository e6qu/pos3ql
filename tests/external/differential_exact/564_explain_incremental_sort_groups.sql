-- PostgreSQL 18 EXPLAIN ANALYZE of an Incremental Sort (show_incremental_sort_info):
-- the batches nodeIncrementalSort.c sorts, at least 32 rows and the rest of
-- the last one's prefix group in each full sort, and past 64 rows each
-- further prefix group on its own as a presorted group. Memory figures are
-- the allocator's own and are left out.
CREATE TABLE t564 (a int, b int);
INSERT INTO t564 SELECT g % 4, g FROM generate_series(1, 200) g;
CREATE TABLE u564 (a int, b int);
INSERT INTO u564 SELECT CASE WHEN g <= 10 THEN 1 WHEN g <= 40 THEN 2 WHEN g <= 300 THEN 3 WHEN g <= 310 THEN 4 ELSE 5 + g % 3 END, g FROM generate_series(1, 700) g;
ANALYZE t564;
ANALYZE u564;
CREATE FUNCTION analyzed564(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    CONTINUE WHEN line ~ '^\s*(Storage|Sort Method):';
    RETURN NEXT regexp_replace(line, '  Average Memory: \d+kB  Peak Memory: \d+kB', '');
  END LOOP;
END
$$;
SELECT analyzed564('SELECT a, b, row_number() OVER (PARTITION BY a) FROM t564 ORDER BY a, b');
SELECT analyzed564('SELECT a, b, sum(b) OVER (ORDER BY a) FROM t564 ORDER BY a, b DESC');
SELECT analyzed564('SELECT a, b, count(*) OVER (PARTITION BY a) FROM u564 ORDER BY a, b DESC');
SELECT analyzed564('SELECT a, b, count(*) OVER (PARTITION BY a) FROM u564 WHERE a < 3 ORDER BY a, b DESC');
SELECT analyzed564('SELECT a, b, count(*) OVER (PARTITION BY a) FROM u564 WHERE a > 4 ORDER BY a, b DESC');
SELECT a, b, count(*) OVER (PARTITION BY a) FROM u564 WHERE a < 3 ORDER BY a, b DESC LIMIT 5;
