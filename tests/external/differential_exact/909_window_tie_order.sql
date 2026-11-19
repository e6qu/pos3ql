-- PostgreSQL 18 returns the rows of a window's sort that tie on its keys in
-- the order pg_qsort leaves them (insertion sort below seven rows, a
-- median-of-three partition above), and numbers them in that order: row_number,
-- rank over an order key that ties, a running sum in a ROWS frame, and a
-- later window sorting the rows the first returned.
CREATE TABLE t909 (a int, b int);
INSERT INTO t909 SELECT g % 5, g FROM generate_series(1, 150) g;
SELECT a, b, row_number() OVER (ORDER BY a) FROM t909;
SELECT a, b, rank() OVER (PARTITION BY a ORDER BY b % 3) FROM t909;
SELECT a, b, sum(b) OVER (ORDER BY a ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t909;
SELECT a, b, count(*) OVER (ORDER BY a) FROM t909 ORDER BY a;
SELECT a, b, row_number() OVER () FROM t909 ORDER BY a LIMIT 20;
SELECT a, b, row_number() OVER (ORDER BY a), rank() OVER (ORDER BY b % 4) FROM t909;
SELECT a, b, dense_rank() OVER (ORDER BY a % 2), sum(b) OVER (ORDER BY a % 3) FROM t909;
