-- Grouping sets are planned as PostgreSQL's `consider_groupingsets_paths`
-- plans them: the sets chained into rollups (`extract_rollup_sets`,
-- `reorder_grouping_sets`), every set hashed when the tables fit in
-- `hash_mem` (a `MixedAggregate` when an empty set needs the sorted phase),
-- or the input sorted for the first rollup with the other rollups hashed or
-- sorted again, each node listing its `Hash Key`, `Group Key` and
-- `Sort Key` lines.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/planner.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/explain.c
CREATE TABLE t802 (a int, b int, c text, d int);
INSERT INTO t802 SELECT g % 10, g % 7, 'v' || (g % 5), g FROM generate_series(1, 3000) g;
ANALYZE t802;
EXPLAIN SELECT a, b, count(*) FROM t802 GROUP BY GROUPING SETS ((a), (b));
EXPLAIN SELECT a, b, count(*) FROM t802 GROUP BY ROLLUP (a, b);
EXPLAIN SELECT a, b, count(*) FROM t802 GROUP BY CUBE (a, b);
EXPLAIN SELECT a, count(*) FROM t802 GROUP BY GROUPING SETS ((a), ());
EXPLAIN SELECT a, b, count(*) FROM t802 GROUP BY ROLLUP (a, b) ORDER BY a, b;
EXPLAIN SELECT d, count(*) FROM t802 GROUP BY ROLLUP (d);
EXPLAIN SELECT a, c, sum(d) FROM t802 GROUP BY GROUPING SETS ((a, c), (c), ());
EXPLAIN SELECT count(*) FROM t802 GROUP BY GROUPING SETS ((), ());
EXPLAIN SELECT a, b, c, count(*) FROM t802 GROUP BY GROUPING SETS ((a, b), (b, c), (a), (c));
EXPLAIN SELECT a, b, count(*) FROM t802 GROUP BY DISTINCT ROLLUP (a, b), ROLLUP (a);
EXPLAIN (COSTS OFF) SELECT a, b, count(*) FROM t802 GROUP BY ROLLUP (b, a) ORDER BY b DESC;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, b, count(*) FROM t802 GROUP BY CUBE (a, b);
EXPLAIN (FORMAT JSON, COSTS OFF) SELECT a, b, count(*) FROM t802 GROUP BY ROLLUP (a, b);
SET enable_hashagg = off;
EXPLAIN SELECT a, b, count(*) FROM t802 GROUP BY CUBE (a, b);
EXPLAIN SELECT a, b, count(*) FROM t802 GROUP BY GROUPING SETS ((a), (b));
EXPLAIN (FORMAT JSON, COSTS OFF) SELECT a, b, count(*) FROM t802 GROUP BY CUBE (a, b);
RESET enable_hashagg;
SET work_mem = '64kB';
EXPLAIN SELECT d, a, count(*) FROM t802 GROUP BY GROUPING SETS ((d), (a), (d, a), ());
EXPLAIN SELECT a, b, d, count(*) FROM t802 GROUP BY ROLLUP (a, b), ROLLUP (d);
RESET work_mem;
CREATE TABLE u802 (a int, z int);
INSERT INTO u802 SELECT g, g FROM generate_series(1, 20) g;
ANALYZE u802;
EXPLAIN (COSTS OFF) SELECT a, b, count(*) FROM t802 GROUP BY ROLLUP (a, b) HAVING count(*) > 1;
EXPLAIN (COSTS OFF) SELECT a + 1, count(*) FROM t802 GROUP BY CUBE (a + 1, b);
EXPLAIN (COSTS OFF) SELECT t802.a, z, count(*) FROM t802 JOIN u802 ON t802.a = u802.a GROUP BY GROUPING SETS ((t802.a), (z));
SELECT a, b, count(*) FROM t802 GROUP BY ROLLUP (a, b) HAVING count(*) > 400 ORDER BY 1, 2;
SELECT a, b, count(*), sum(d) FROM t802 GROUP BY CUBE (a, b) ORDER BY a, b;
SELECT a, c, sum(d) FROM t802 GROUP BY GROUPING SETS ((a, c), (c), ()) ORDER BY a, c;
