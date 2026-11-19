-- A window clause above lag, avg or a sum of bigint values holds results the
-- lower clauses' sizes alone cannot predict; its sort still charges the real
-- tuples, spills at work_mem, and returns tied rows in the heap merge's order.
SET work_mem = '64kB';
CREATE TABLE t912 (a int, b int, c bigint, n numeric);
INSERT INTO t912 SELECT g % 5, g % 11, g * 100000000000, (g % 13) + g / 7.0 FROM generate_series(1, 1000) g;
SELECT a, b, avg(b) OVER (PARTITION BY a ORDER BY b), row_number() OVER (ORDER BY b, c % 3) FROM t912;
SELECT a, b, sum(c) OVER (PARTITION BY b ORDER BY a), rank() OVER (PARTITION BY a ORDER BY n) FROM t912;
SELECT a, b, lag(n) OVER (PARTITION BY a ORDER BY b), lead(b) OVER (PARTITION BY b ORDER BY a) FROM t912;
SELECT a, b, avg(n) OVER (PARTITION BY a), row_number() OVER (PARTITION BY b ORDER BY a) FROM t912;
