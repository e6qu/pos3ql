-- Aggregation is costed as PostgreSQL's `cost_agg` costs it over the chosen
-- scan of one table: each transition state's function and inputs per input
-- row and each final function once per group (`get_agg_clause_costs`),
-- sorted and hashed grouping weighed against each other with the hash
-- table's spill model (`hash_agg_entry_size`, `hash_agg_set_limits`).
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/costsize.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/prep/prepagg.c
CREATE TABLE t790 (a int, b int, c text, d numeric, e bool);
INSERT INTO t790 SELECT g, g % 7, 'v' || (g % 50), g / 3.0, g % 2 = 0 FROM generate_series(1, 3000) g;
CREATE INDEX t790_a ON t790 (a);
ANALYZE t790;
EXPLAIN SELECT count(*), sum(a), avg(d), max(c) FROM t790;
EXPLAIN SELECT min(b), max(b), count(c) FROM t790 WHERE b > 2;
EXPLAIN SELECT sum(a), avg(a) FROM t790;
EXPLAIN SELECT sum(d), avg(d), sum(d) FROM t790;
EXPLAIN SELECT count(*) FILTER (WHERE b > 3) FROM t790;
EXPLAIN SELECT sum(a + b) FROM t790;
EXPLAIN SELECT b, count(*) FROM t790 GROUP BY b;
EXPLAIN SELECT c, sum(d) FROM t790 GROUP BY c ORDER BY c;
EXPLAIN SELECT c, b, count(*) FROM t790 GROUP BY c, b;
EXPLAIN SELECT c, b, count(*) FROM t790 GROUP BY c, b ORDER BY b DESC, c;
EXPLAIN SELECT b, count(*) FROM t790 GROUP BY b ORDER BY count(*) DESC;
EXPLAIN SELECT e, avg(a) FROM t790 GROUP BY e;
EXPLAIN SELECT d, count(*) FROM t790 GROUP BY d;
EXPLAIN SELECT d, count(*) FROM t790 GROUP BY d ORDER BY d;
EXPLAIN SELECT b, count(*) FROM t790 WHERE c = 'v3' GROUP BY 1;
SET enable_hashagg = off;
EXPLAIN SELECT b, count(*) FROM t790 GROUP BY b;
EXPLAIN SELECT c, b, count(*) FROM t790 GROUP BY c, b ORDER BY b DESC, c;
RESET enable_hashagg;
SET enable_sort = off;
EXPLAIN SELECT d, count(*) FROM t790 GROUP BY d ORDER BY d;
RESET enable_sort;
SET work_mem = '64kB';
EXPLAIN SELECT d, count(*) FROM t790 GROUP BY d;
EXPLAIN SELECT d, c, count(*), sum(a) FROM t790 GROUP BY d, c;
RESET work_mem;
SELECT b, count(*), sum(a) FROM t790 GROUP BY b ORDER BY b;
SELECT count(*), sum(a), avg(d)::numeric(10,3), max(c) FROM t790;
EXPLAIN (FORMAT JSON) SELECT d, count(*) FROM t790 GROUP BY d;
SET work_mem = '64kB';
EXPLAIN (FORMAT JSON) SELECT d, count(*) FROM t790 GROUP BY d;
RESET work_mem;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT count(*), max(c) FROM t790;
