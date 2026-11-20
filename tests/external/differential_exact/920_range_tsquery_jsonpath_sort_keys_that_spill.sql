-- Ranges, multiranges, tsquery, jsonpath and snapshot values are charged at
-- their PostgreSQL sizes, so a sort of them spills where PostgreSQL's does
-- and its tied rows follow the merge of the runs.
CREATE TABLE t920 (a int, r int4range, m int4multirange, q tsquery, p jsonpath);
INSERT INTO t920 SELECT g % 7, int4range(g % 5, g % 5 + 3), int4multirange(int4range(g % 4, g % 4 + 2), int4range(10, 12)), to_tsquery('simple', 'w' || (g % 3) || ' & x'), ('$.k' || (g % 4) || '[0]')::jsonpath FROM generate_series(1, 1500) g;
SET work_mem = '64kB';
SELECT a, r FROM t920 ORDER BY a OFFSET 700 LIMIT 20;
SELECT a, m FROM t920 ORDER BY a OFFSET 700 LIMIT 20;
SELECT a, q FROM t920 ORDER BY a OFFSET 700 LIMIT 20;
SELECT a, p FROM t920 ORDER BY a OFFSET 700 LIMIT 20;
SELECT a, r, m FROM t920 ORDER BY r OFFSET 700 LIMIT 20;
SET work_mem = '256kB';
SELECT a, r, m FROM t920 ORDER BY a OFFSET 700 LIMIT 20;
SELECT a, r, m, q, p FROM t920 ORDER BY a OFFSET 700 LIMIT 20;
