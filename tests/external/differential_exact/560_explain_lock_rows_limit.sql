-- PostgreSQL 18 LockRows under a Limit: the Limit pulls only the rows it
-- returns, so a locking query read in scan order stops its scan once the
-- limit has its rows; a sorted one locks the sorted rows the Limit reads.
-- The sort's tuples carry the ctid the lock reads.
CREATE TABLE t560 (a int, b int);
INSERT INTO t560 SELECT g % 5, g FROM generate_series(1, 99) g;
ANALYZE t560;
CREATE FUNCTION analyzed560(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    RETURN NEXT line;
  END LOOP;
END
$$;
SELECT analyzed560('SELECT * FROM t560 LIMIT 3 FOR UPDATE');
SELECT analyzed560('SELECT * FROM t560 WHERE a = 1 LIMIT 3 FOR UPDATE');
SELECT analyzed560('SELECT * FROM t560 OFFSET 2 LIMIT 3 FOR SHARE');
SELECT analyzed560('SELECT b FROM t560 WHERE b > 90 FOR NO KEY UPDATE LIMIT 20');
SELECT analyzed560('SELECT * FROM t560 ORDER BY b LIMIT 3 FOR UPDATE');
SELECT analyzed560('SELECT * FROM t560 LIMIT 0 FOR UPDATE');
SELECT * FROM t560 WHERE a = 1 LIMIT 3 FOR UPDATE;
SELECT * FROM t560 OFFSET 97 LIMIT 5 FOR KEY SHARE;
BEGIN;
SELECT b FROM t560 WHERE a = 2 LIMIT 2 FOR UPDATE SKIP LOCKED;
SELECT b FROM t560 WHERE a = 2 FOR UPDATE NOWAIT;
COMMIT;
