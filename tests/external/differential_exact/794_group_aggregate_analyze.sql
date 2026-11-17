-- A `GroupAggregate` reads its input from a `Sort` by the grouping keys, and
-- `EXPLAIN ANALYZE` counts both as PostgreSQL's executor runs them: the sort
-- returns every input row and the aggregate one row per group, in the
-- sort's key order. The sort's memory figure is PostgreSQL's allocator's, so
-- that line is left out.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeAgg.c
CREATE TABLE t794 (a int, b int, c text, d numeric);
INSERT INTO t794 SELECT g, g % 7, 'v' || (g % 50), g / 3.0 FROM generate_series(1, 3000) g;
ANALYZE t794;
CREATE FUNCTION analyzed794(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    CONTINUE WHEN line ~ '^\s*(Sort Method|Batches|Storage):';
    RETURN NEXT line;
  END LOOP;
END
$$;
SELECT analyzed794('SELECT d, count(*) FROM t794 GROUP BY d ORDER BY d');
SELECT analyzed794('SELECT d, count(*) FROM t794 WHERE b = 2 GROUP BY d ORDER BY d');
SET enable_hashagg = off;
SELECT analyzed794('SELECT b, count(*) FROM t794 GROUP BY b');
SELECT analyzed794('SELECT c, b, count(*) FROM t794 GROUP BY c, b ORDER BY b DESC, c');
SELECT b, count(*), sum(a) FROM t794 GROUP BY b;
SELECT c, b, count(*) FROM t794 WHERE a < 120 GROUP BY c, b ORDER BY b DESC LIMIT 12;
RESET enable_hashagg;
CREATE FUNCTION json794(query text) RETURNS jsonb LANGUAGE plpgsql AS $$
DECLARE
  plan json;
BEGIN
  EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF, FORMAT JSON) ' || query INTO plan;
  RETURN (plan::jsonb)->0->'Plan' #- '{Plans,0,Sort Space Used}';
END
$$;
SELECT jsonb_pretty(json794('SELECT d, count(*) FROM t794 WHERE a < 30 GROUP BY d ORDER BY d'));
