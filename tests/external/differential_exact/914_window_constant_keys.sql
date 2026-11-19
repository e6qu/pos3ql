-- An integer in a window's PARTITION BY or ORDER BY is a constant, never an
-- output-column position (unlike the query's own ORDER BY), with and without
-- a FROM clause.
SELECT sum(1) OVER (PARTITION BY 1 ORDER BY 1 ROWS BETWEEN 1 PRECEDING AND CURRENT ROW);
SELECT row_number() OVER (PARTITION BY 1 ORDER BY 1), count(*) OVER (ORDER BY 2);
CREATE TABLE t914 (a int, b int);
INSERT INTO t914 VALUES (1, 20), (2, 10), (3, 30);
SELECT a, sum(b) OVER (PARTITION BY 1 ORDER BY 1) FROM t914 ORDER BY a;
SELECT a, row_number() OVER (PARTITION BY 2 ORDER BY 1) FROM t914 ORDER BY 1;
SELECT b, a, rank() OVER (ORDER BY 1 DESC) FROM t914 ORDER BY 2;
SELECT b, sum(a) OVER (ORDER BY b) FROM t914 ORDER BY 1;
