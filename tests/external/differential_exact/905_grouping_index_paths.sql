-- The rows of each statement stay under the forced-spill run's 256 KiB memtable.
-- A grouping reads the rows of an index in the order of its grouping keys as
-- PostgreSQL's `add_paths_to_grouping_rel` does: a `GroupAggregate` over an
-- index scan, an incremental sort over the rows an index orders by the first
-- keys, the keys reordered to the order an index returns them
-- (`get_useful_group_keys_orderings`), and a key an equality fixes to a
-- constant left out of the keys.
CREATE TABLE g905 (a int, b int, c text, d numeric);
INSERT INTO g905 SELECT g, g % 7, 'v' || (g % 50), g / 3.0 FROM generate_series(1, 2000) g;
CREATE INDEX g905_a ON g905 (a);
CREATE INDEX g905_b ON g905 (b);
CREATE INDEX g905_bc ON g905 (b, c);
CREATE INDEX g905_dd ON g905 (d DESC NULLS LAST);
ANALYZE g905;
EXPLAIN SELECT a, count(*) FROM g905 GROUP BY a;
EXPLAIN SELECT b, count(*) FROM g905 GROUP BY b;
EXPLAIN SELECT b, c, count(*) FROM g905 GROUP BY b, c;
EXPLAIN SELECT c, b, count(*) FROM g905 GROUP BY c, b;
EXPLAIN SELECT b, count(*) FROM g905 GROUP BY b ORDER BY b DESC;
EXPLAIN SELECT b, d, count(*) FROM g905 GROUP BY b, d;
EXPLAIN SELECT d, b, count(*) FROM g905 GROUP BY d, b;
EXPLAIN SELECT a, count(*) FROM g905 GROUP BY a LIMIT 5;
EXPLAIN SELECT b, sum(a) FROM g905 WHERE a > 2500 GROUP BY b;
EXPLAIN SELECT b, c, sum(a) FROM g905 GROUP BY c, b LIMIT 3;
EXPLAIN SELECT b, count(*) FROM g905 GROUP BY b HAVING count(*) > 3;
EXPLAIN SELECT d, count(*) FROM g905 GROUP BY d ORDER BY d DESC;
SET enable_hashagg = off;
EXPLAIN SELECT b, count(*) FROM g905 GROUP BY b;
EXPLAIN SELECT c, b, count(*) FROM g905 GROUP BY c, b;
EXPLAIN SELECT c, b, d, count(*) FROM g905 GROUP BY c, b, d;
EXPLAIN SELECT b, c, d, count(*) FROM g905 GROUP BY b, c, d;
SET enable_incremental_sort = off;
EXPLAIN SELECT b, c, d, count(*) FROM g905 GROUP BY b, c, d;
EXPLAIN SELECT c, b, count(*) FROM g905 GROUP BY c, b;
SET enable_indexscan = off;
EXPLAIN SELECT b, count(*) FROM g905 GROUP BY b;
SET enable_sort = off;
EXPLAIN SELECT b, count(*) FROM g905 GROUP BY b;
RESET enable_hashagg; RESET enable_incremental_sort; RESET enable_indexscan; RESET enable_sort;
EXPLAIN SELECT b, c, count(*) FROM g905 WHERE b = 3 GROUP BY b, c;
EXPLAIN SELECT b, c, count(*) FROM g905 WHERE b = 3 GROUP BY c, b;
EXPLAIN SELECT b, c, count(*) FROM g905 WHERE c = 'v3' GROUP BY b, c ORDER BY b;
SET enable_hashagg = off;
EXPLAIN SELECT b, c, count(*) FROM g905 WHERE b = 3 GROUP BY b, c;
EXPLAIN SELECT b, c, sum(a) FROM g905 WHERE a > 2500 GROUP BY b, c ORDER BY c, b;
EXPLAIN SELECT a, count(*) FROM g905 WHERE a < 100 GROUP BY a;
SELECT b, c, count(*) FROM g905 WHERE b = 3 GROUP BY b, c ORDER BY c LIMIT 5;
SELECT b, count(*), sum(a) FROM g905 GROUP BY b ORDER BY b;
SELECT c, b, count(*) FROM g905 GROUP BY c, b ORDER BY b, c LIMIT 8;
