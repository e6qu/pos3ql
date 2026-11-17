-- `min` and `max` of a column an index orders read the first row of the
-- index scan in an InitPlan, nulls excluded and the query's conditions
-- applied, when that costs less than aggregating the table
-- (`preprocess_minmax_aggregates`, `create_minmaxagg_path`).
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/planagg.c
CREATE TABLE t791 (a int, b int, c text, d numeric);
INSERT INTO t791 SELECT CASE WHEN g % 50 = 0 THEN NULL ELSE g END, g % 10, 'x' || g, g / 7.0 FROM generate_series(1, 2000) g;
CREATE INDEX t791_a ON t791 (a);
CREATE INDEX t791_b ON t791 (b);
CREATE INDEX t791_c ON t791 (c);
ANALYZE t791;
EXPLAIN SELECT max(a) FROM t791;
EXPLAIN SELECT min(a), max(a), min(c) FROM t791;
EXPLAIN SELECT max(a) FROM t791 WHERE b = 3;
EXPLAIN SELECT max(a) FROM t791 WHERE a < 100;
EXPLAIN SELECT max(d) FROM t791;
EXPLAIN SELECT max(a), count(*) FROM t791;
EXPLAIN SELECT max(DISTINCT a) FROM t791;
EXPLAIN SELECT max(a) FILTER (WHERE b = 1) FROM t791;
EXPLAIN SELECT max(a) AS top, max(a) * 2 FROM t791;
EXPLAIN SELECT max(a) FROM ONLY t791;
EXPLAIN VERBOSE SELECT min(a), max(c) FROM t791;
SELECT max(a), min(a), min(c), max(c) FROM t791;
SELECT max(a) FROM t791 WHERE b = 3;
SELECT max(a) AS top, max(a) * 2 FROM t791;
SELECT max(a) FROM t791 WHERE a > 5000;
SET enable_indexscan = off;
EXPLAIN SELECT max(a) FROM t791;
RESET enable_indexscan;
DELETE FROM t791;
SELECT max(a), min(c) FROM t791;
