-- A sort under a limit over a large join keeps only the rows the limit
-- returns once it forms its bounded heap: seven 8-row tables cross joined
-- (2,097,152 rows) sort in a fixed amount of memory.
CREATE TABLE tn1 (a int, b text);
INSERT INTO tn1 SELECT g, 'x' || g FROM generate_series(1, 8) g;
CREATE TABLE tn2 AS SELECT * FROM tn1;
CREATE TABLE tn3 AS SELECT * FROM tn1;
CREATE TABLE tn4 AS SELECT * FROM tn1;
CREATE TABLE tn5 AS SELECT * FROM tn1;
CREATE TABLE tn6 AS SELECT * FROM tn1;
CREATE TABLE tn7 AS SELECT * FROM tn1;
ANALYZE tn1, tn2, tn3, tn4, tn5, tn6, tn7;
EXPLAIN (COSTS OFF)
  SELECT * FROM tn1, tn2, tn3, tn4, tn5, tn6, tn7 ORDER BY 1 LIMIT 1;
SELECT * FROM tn1, tn2, tn3, tn4, tn5, tn6, tn7 ORDER BY 1 LIMIT 1;
SELECT * FROM tn1, tn2, tn3, tn4, tn5, tn6 ORDER BY 12 DESC, 2 LIMIT 2 OFFSET 1;
-- Ties keep the order PostgreSQL's heap leaves them in, and EXPLAIN ANALYZE
-- reports the heap's memory.
CREATE TABLE tn_rows (a int, b text, c int);
INSERT INTO tn_rows SELECT g % 37, repeat('x', g % 50), g FROM generate_series(1, 5000) g;
ANALYZE tn_rows;
SELECT a, b, c FROM tn_rows ORDER BY a, length(b) LIMIT 5;
SELECT a, c FROM tn_rows ORDER BY a DESC LIMIT 7 OFFSET 3;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT tn_rows.a, tn1.b FROM tn_rows, tn1 ORDER BY tn1.b, tn_rows.c DESC LIMIT 400;
