-- EXPLAIN ANALYZE of windows over a join counts the plan PostgreSQL's join
-- search chose, whose nodes produce the windows' input. BUFFERS is on by
-- default under ANALYZE (PostgreSQL 18); a query reading no relation counts
-- none.
CREATE TABLE wa935 (x int, k int);
CREATE TABLE wb935 (y int, k int);
INSERT INTO wa935 SELECT g, g % 50 FROM generate_series(1, 600) g;
INSERT INTO wb935 SELECT g % 40, g FROM generate_series(1, 300) g;
ANALYZE wa935, wb935;
CREATE FUNCTION analyzed935(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    -- A window's tuplestore and the sorts under it are not sized as
    -- PostgreSQL's yet (PLAN.md), so their memory figures are masked.
    RETURN NEXT regexp_replace(line, '(Memory|Maximum Storage): \d+kB', '\1: NkB');
  END LOOP;
END
$$;
SELECT analyzed935('SELECT sum(wa935.x) OVER (ORDER BY wa935.x, wb935.k) FROM wa935, wb935 WHERE wa935.k = wb935.k');
SELECT analyzed935('SELECT wa935.x, rank() OVER (PARTITION BY wb935.y ORDER BY wa935.x) FROM wa935 JOIN wb935 ON wa935.k = wb935.k WHERE wb935.y < 5');
SELECT analyzed935('SELECT sum(wa935.x) OVER (PARTITION BY wa935.k) FROM wa935 LEFT JOIN wb935 ON wa935.k = wb935.k ORDER BY wa935.x LIMIT 4');
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) SELECT 1 + 1;
SELECT sum(wa935.x) OVER (ORDER BY wa935.x, wb935.k) FROM wa935, wb935 WHERE wa935.k = wb935.k
  ORDER BY 1 DESC LIMIT 3;
DROP FUNCTION analyzed935(text);
DROP TABLE wa935, wb935;
