-- ORDER BY over a sequential scan sorts as PostgreSQL's tuplesort does: one
-- quicksort while the tuples fit work_mem, quicksorted runs merged through
-- tapes once they outgrow it, and a top-N heap under a limit. Rows with equal
-- keys come out in that machine's order, which changes with work_mem.
CREATE TABLE t915 (a int, b int, c int);
INSERT INTO t915 SELECT g % 13, g, (g * 7) % 11 FROM generate_series(1, 5000) g;
SET work_mem = '64kB';
SELECT a, b FROM t915 ORDER BY a OFFSET 100 LIMIT 30;
SELECT a, b FROM t915 ORDER BY a OFFSET 3000 LIMIT 30;
SELECT a, b, c FROM t915 ORDER BY a, c OFFSET 3000 LIMIT 30;
SELECT a, b FROM t915 ORDER BY a LIMIT 500 OFFSET 100;
SELECT a, b FROM t915 ORDER BY a LIMIT 3000 OFFSET 2000;
SET work_mem = '128kB';
SELECT a, b, c FROM t915 ORDER BY c OFFSET 3000 LIMIT 30;
SELECT a, b FROM t915 ORDER BY a OFFSET 4970;
SELECT a, b FROM t915 ORDER BY a DESC, c OFFSET 2500 LIMIT 40;
SET work_mem = '192kB';
SELECT a, b, c FROM t915 ORDER BY c OFFSET 3000 LIMIT 30;
SET work_mem = '1MB';
SELECT a, b, c FROM t915 ORDER BY c OFFSET 3000 LIMIT 30;
SELECT a, b FROM t915 ORDER BY a LIMIT 20;
