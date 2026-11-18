-- EXPLAIN ANALYZE of an Incremental Sort of a join's rows: each batch's
-- tuplesort reports its method and the most memory the sort held so far
-- (instrumentSortedGroup, tuplesort_get_stats), a limit's bound turning a
-- large batch into a top-N heapsort, as show_incremental_sort_info prints
-- them.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeIncrementalSort.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/explain.c
CREATE TABLE ia875 (id int, k int, v int, w text);
CREATE TABLE ib875 (id int, k int, v int, w text);
INSERT INTO ia875 SELECT g, g % 40, g % 7, 'a' || (g % 13) FROM generate_series(1, 800) g;
INSERT INTO ib875 SELECT g, g % 40, g % 5, 'b' || (g % 11) FROM generate_series(1, 400) g;
CREATE INDEX ia875_k ON ia875 (k);
CREATE INDEX ib875_k ON ib875 (k);
ANALYZE ia875; ANALYZE ib875;
EXPLAIN SELECT ia875.id, ib875.id FROM ia875 JOIN ib875 ON ia875.k = ib875.k ORDER BY ia875.k, ib875.w LIMIT 50;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT ia875.id, ib875.id FROM ia875 JOIN ib875 ON ia875.k = ib875.k ORDER BY ia875.k, ib875.w LIMIT 50;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT ia875.id, ib875.id, ib875.w FROM ia875 JOIN ib875 ON ia875.k = ib875.k ORDER BY ia875.k, ib875.w, ia875.id LIMIT 300;
SELECT ia875.id, ib875.id, ib875.w FROM ia875 JOIN ib875 ON ia875.k = ib875.k ORDER BY ia875.k, ib875.w LIMIT 30;
