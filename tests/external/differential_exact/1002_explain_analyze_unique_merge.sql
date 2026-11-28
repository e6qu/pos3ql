-- EXPLAIN ANALYZE of a semi join whose inner relation is made unique: a
-- HashAggregate reports its batches and memory, a Unique over a Sort the
-- sort's method and memory. A merge join reading an index scan in order
-- with a filter counts the rows its filter removed as far as the join
-- fetched.
CREATE TABLE uo (id int, k int);
CREATE TABLE ui (k int, v int);
INSERT INTO uo SELECT g, g % 1000 FROM generate_series(1, 3000) g;
INSERT INTO ui SELECT g % 20, g FROM generate_series(1, 2000) g;
CREATE INDEX uo_k ON uo (k);
ANALYZE uo, ui;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT count(*) FROM uo WHERE k IN (SELECT k FROM ui WHERE v < 500);
SELECT count(*) FROM uo WHERE k IN (SELECT k FROM ui WHERE v < 500);
SET enable_hashagg = off;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT count(*) FROM uo WHERE k IN (SELECT k FROM ui WHERE v < 500);
RESET enable_hashagg;
SET enable_mergejoin = off;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT uo.id FROM uo WHERE k IN (SELECT k FROM ui WHERE v < 500) ORDER BY 1 LIMIT 4;
-- A scan parameterized by a semi join's unique-ified inner relation
-- repeats once per unique row (`adjust_rowcount_for_semijoins`).
SET enable_hashjoin = off;
EXPLAIN SELECT uo.id FROM uo WHERE k IN (SELECT k FROM ui WHERE v < 500) ORDER BY 1 LIMIT 4;
RESET enable_hashjoin;
RESET enable_mergejoin;
CREATE INDEX ui_k ON ui (k);
SET enable_hashjoin = off;
SET enable_nestloop = off;
SET enable_sort = off;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT count(*) FROM uo JOIN ui ON ui.k = uo.k WHERE ui.v % 3 = 0;
RESET ALL;
