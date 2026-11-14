-- PostgreSQL 18 explain.c ExplainNode with ANALYZE: every node's actual
-- rows and loops, `(never executed)` for nodes that did not run, the rows
-- each qualification removed, and the inner loops of a nested loop. The
-- memory and hash sizes PostgreSQL measures for sorts, hashes, aggregates
-- and materialized rows are its own allocator's, so those lines are left out.
CREATE TABLE t430 (a int, b int);
INSERT INTO t430 SELECT g % 10, g FROM generate_series(1, 100) g;
CREATE TABLE u430 (a int, c text);
INSERT INTO u430 SELECT g, 'x' || g FROM generate_series(1, 20) g;
CREATE TABLE k430 (k int PRIMARY KEY, v int);
INSERT INTO k430 SELECT g, g * 2 FROM generate_series(1, 5) g;
ANALYZE t430;
ANALYZE u430;
ANALYZE k430;
CREATE FUNCTION analyzed430(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    CONTINUE WHEN line ~ '^\s*(Buckets|Sort Method|Batches|Storage):';
    RETURN NEXT line;
  END LOOP;
END
$$;
SELECT analyzed430('SELECT * FROM t430 WHERE a > 5');
SELECT analyzed430('SELECT * FROM t430 WHERE a > 5 LIMIT 3');
SELECT analyzed430('SELECT * FROM t430 LIMIT 0');
SELECT analyzed430('SELECT * FROM t430 OFFSET 95');
SELECT analyzed430('SELECT * FROM t430 JOIN u430 ON t430.b = u430.a WHERE u430.a < 10');
SELECT analyzed430('SELECT a, count(*) FROM t430 GROUP BY a ORDER BY a LIMIT 3');
SELECT analyzed430('SELECT a, count(*) FROM t430 GROUP BY a HAVING count(*) > 100');
SELECT analyzed430('SELECT count(*) FROM t430');
SELECT analyzed430('SELECT * FROM t430 ORDER BY b DESC LIMIT 2');
SELECT analyzed430('SELECT * FROM t430 WHERE b > 95 ORDER BY b');
SELECT analyzed430('SELECT DISTINCT a FROM t430');
SELECT analyzed430('SELECT * FROM k430 x, k430 y WHERE x.k < y.k');
SELECT analyzed430('SELECT a FROM t430 UNION ALL SELECT a FROM u430 UNION ALL SELECT k FROM k430');
SELECT analyzed430('SELECT a FROM t430 UNION SELECT a FROM u430');
SELECT analyzed430('SELECT * FROM (SELECT a, b FROM t430 ORDER BY b LIMIT 5) s');
SELECT analyzed430('SELECT s.a FROM (SELECT a, b FROM t430 ORDER BY b LIMIT 5) s WHERE s.a > 2');
SELECT analyzed430('SELECT * FROM (VALUES (1, ''a''), (2, ''b''), (3, ''c'')) v(x, y) WHERE x > 1');
SELECT analyzed430('SELECT * FROM generate_series(1, 10) g WHERE g > 7');
SELECT analyzed430('SELECT * FROM t430 WHERE false');
SELECT analyzed430('VALUES (1), (2)');
SELECT analyzed430('UPDATE t430 SET b = b WHERE a = 1');
SELECT analyzed430('DELETE FROM t430 WHERE a = 42');
SELECT analyzed430('INSERT INTO k430 SELECT k + 10, v FROM k430 WHERE k < 3');
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF, FORMAT JSON)
  SELECT * FROM t430 WHERE a > 5;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF, FORMAT YAML)
  SELECT * FROM t430 LIMIT 0;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF, FORMAT XML)
  SELECT count(*) FROM t430 WHERE a > 5;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF, FORMAT JSON)
  DELETE FROM t430 WHERE a = 42;
