-- PostgreSQL 18 explain.c ExplainNode with ANALYZE over the queries rewrite
-- rules turn a statement into, each counted as its own plan, and over a
-- query without FROM: its one-row Result with the WHERE condition as a
-- one-time filter, the ProjectSet expanding set-returning calls, and only
-- the sort, grouping and distinct keys that are not planner constants.
CREATE TABLE t434 (a int, b int);
INSERT INTO t434 SELECT g % 10, g FROM generate_series(1, 100) g;
CREATE TABLE u434 (a int, c text);
CREATE TABLE l434 (a int, n int);
ANALYZE t434;
CREATE FUNCTION analyzed434(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    CONTINUE WHEN line ~ '^\s*(Buckets|Sort Method|Batches|Storage):';
    RETURN NEXT line;
  END LOOP;
END
$$;
CREATE RULE l434_ins AS ON INSERT TO l434 DO ALSO INSERT INTO u434 VALUES (new.a, 'r');
SELECT analyzed434('INSERT INTO l434 SELECT a, b FROM t434 WHERE b < 4');
SELECT analyzed434('INSERT INTO l434 VALUES (5, 5), (6, 6)');
SELECT analyzed434('INSERT INTO l434 VALUES (7, 7)');
SELECT analyzed434('INSERT INTO t434 VALUES (7, 7)');
SELECT analyzed434('INSERT INTO t434 VALUES (8, 8), (9, 9)');
SELECT analyzed434('SELECT 1');
SELECT analyzed434('SELECT 1 ORDER BY 1');
SELECT analyzed434('SELECT random() ORDER BY 1');
SELECT analyzed434('SELECT DISTINCT random()');
SELECT analyzed434('SELECT DISTINCT 1, 2 ORDER BY 1');
SELECT analyzed434('SELECT count(*) HAVING count(*) > 1');
SELECT analyzed434('SELECT count(*) WHERE false');
SELECT analyzed434('SELECT 1 WHERE random() > 2');
SELECT analyzed434('SELECT 1 WHERE 1 > 0');
SELECT analyzed434('SELECT 1 OFFSET 1');
SELECT analyzed434('SELECT 1 LIMIT 0');
SELECT analyzed434('SELECT generate_series(1, 3) ORDER BY 1 DESC');
SELECT analyzed434('SELECT generate_series(1, 3), generate_series(1, 2)');
SELECT analyzed434('SELECT count(*), generate_series(1, 2)');
SELECT analyzed434('SELECT 1 GROUP BY 1');
SELECT analyzed434('SELECT DISTINCT ON (1) 1, 2');
SELECT analyzed434('SELECT generate_series(1, 2), b FROM t434 WHERE b < 3');
EXPLAIN (COSTS OFF) SELECT * FROM t434 WHERE random() > 2;
EXPLAIN (COSTS OFF) SELECT lower(c) FROM u434 GROUP BY 1;
EXPLAIN (COSTS OFF) SELECT a + 1 FROM u434 GROUP BY 1;
EXPLAIN (COSTS OFF) SELECT a + 1, lower(c) FROM u434 ORDER BY 1, 2;
