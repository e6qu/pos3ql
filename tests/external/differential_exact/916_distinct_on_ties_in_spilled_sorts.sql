-- DISTINCT ON keeps the first row of each key after PostgreSQL's sort, so
-- which of several tied rows survives is the tuplesort's order: the quicksort
-- of a sort that fits work_mem, or the merge of the runs of one that spills.
CREATE TABLE t916 (a int, b int, c int);
INSERT INTO t916 SELECT g % 13, g, (g * 7) % 11 FROM generate_series(1, 5000) g;
SET work_mem = '64kB';
SELECT DISTINCT ON (a) a, b, c FROM t916 ORDER BY a;
SELECT DISTINCT ON (c) a, b, c FROM t916 ORDER BY c, b DESC;
SET work_mem = '128kB';
SELECT DISTINCT ON (a) a, b, c FROM t916 ORDER BY a;
SELECT DISTINCT ON (a, c) a, b, c FROM t916 ORDER BY a, c LIMIT 40;
SET work_mem = '4MB';
SELECT DISTINCT ON (a) a, b, c FROM t916 ORDER BY a;
