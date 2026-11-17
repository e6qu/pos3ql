-- cost_agg over one plain table with a HAVING qualification and a LIMIT:
-- the qual's cost (an aggregate costs what a column does) on every group,
-- its selectivity (DEFAULT_EQ_SEL for =, DEFAULT_INEQ_SEL for an
-- inequality, their combinations for AND and OR) on the groups returned,
-- and create_limit_path's share of the path's run cost
-- (adjust_limit_rows_costs), which also picks between the sorted and the
-- hashed grouping. The table stays under the forced-spill run's 256 KiB
-- memtable, also after the earlier fixtures of its shard.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/costsize.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/util/pathnode.c
CREATE TABLE g821 (a int, b int, c text, d numeric);
INSERT INTO g821 SELECT g, g % 18, 'v' || (g % 45), g / 3.0 FROM generate_series(1, 1500) g;
ANALYZE g821;
EXPLAIN SELECT b, count(*) FROM g821 GROUP BY b HAVING count(*) > 50;
EXPLAIN SELECT b, count(*) FROM g821 GROUP BY b HAVING count(*) = 83;
EXPLAIN SELECT b, sum(a) FROM g821 GROUP BY b HAVING sum(a) > 1000 AND max(a) < 1490;
EXPLAIN SELECT b, sum(a) FROM g821 GROUP BY b HAVING sum(a) > 1000 OR count(*) <> 3;
EXPLAIN SELECT c, avg(d) FROM g821 GROUP BY c HAVING avg(d) > 300 ORDER BY c;
EXPLAIN SELECT a, count(*) FROM g821 GROUP BY a HAVING count(*) > 0 ORDER BY a;
EXPLAIN SELECT b, count(*) FROM g821 GROUP BY b LIMIT 5;
EXPLAIN SELECT b, count(*) FROM g821 GROUP BY b ORDER BY b LIMIT 5;
EXPLAIN SELECT b, count(*) FROM g821 GROUP BY b ORDER BY b LIMIT 5 OFFSET 10;
EXPLAIN SELECT a, count(*) FROM g821 GROUP BY a ORDER BY a LIMIT 3;
EXPLAIN SELECT a, count(*) FROM g821 GROUP BY a LIMIT 10 OFFSET 1490;
EXPLAIN SELECT c, count(*) FROM g821 GROUP BY c HAVING count(*) > 30 ORDER BY count(*) DESC, c LIMIT 4;
EXPLAIN SELECT b, count(*) FROM g821 GROUP BY b OFFSET 30;
SET enable_hashagg = off;
EXPLAIN SELECT b, count(*) FROM g821 GROUP BY b HAVING count(*) > 50 LIMIT 2;
RESET enable_hashagg;
SELECT b, count(*) FROM g821 GROUP BY b HAVING count(*) = 83 ORDER BY b;
SELECT c, count(*) FROM g821 GROUP BY c HAVING count(*) > 30 ORDER BY count(*) DESC, c LIMIT 4;
SELECT b, count(*) FROM g821 GROUP BY b ORDER BY b LIMIT 5 OFFSET 10;
