-- A hash join of several batches returning its unmatched inner rows
-- (right and full joins, HJ_FILL_INNER_TUPLES): each batch returns them
-- once its outer rows are probed, in bucket order, batch 0's skew buckets
-- after its main buckets; later batches have no skew buckets.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeHashjoin.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeHash.c
CREATE TABLE ha854 (id int, k int, pad text);
CREATE TABLE hb854 (id int, k int, pad text);
INSERT INTO ha854 SELECT g, g % 400, repeat('a', 100) FROM generate_series(1, 900) g;
INSERT INTO hb854 SELECT g, CASE WHEN g % 9 = 0 THEN NULL ELSE (g * 7) % 900 END, repeat('b', 100) FROM generate_series(1, 500) g;
ANALYZE ha854; ANALYZE hb854;
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SET enable_mergejoin = off;
SET enable_nestloop = off;
EXPLAIN SELECT ha854.id, hb854.id, hb854.pad FROM ha854 RIGHT JOIN hb854 ON ha854.k = hb854.k;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT ha854.id, hb854.id, hb854.pad FROM ha854 RIGHT JOIN hb854 ON ha854.k = hb854.k;
EXPLAIN (COSTS OFF) SELECT ha854.id, hb854.id FROM ha854 RIGHT JOIN hb854 ON ha854.k = hb854.k;
SELECT ha854.id, hb854.id FROM ha854 RIGHT JOIN hb854 ON ha854.k = hb854.k;
SELECT count(*), count(ha854.id), sum(hb854.id) FROM ha854 RIGHT JOIN hb854 ON ha854.k = hb854.k;
EXPLAIN SELECT ha854.id, hb854.id, hb854.pad, ha854.pad FROM ha854 FULL JOIN hb854 ON ha854.k = hb854.k;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT ha854.id, hb854.id, hb854.pad, ha854.pad FROM ha854 FULL JOIN hb854 ON ha854.k = hb854.k;
SELECT ha854.id, hb854.id, hb854.pad, ha854.pad FROM ha854 FULL JOIN hb854 ON ha854.k = hb854.k WHERE ha854.id IS NULL OR hb854.id IS NULL;
SELECT ha854.id, hb854.id FROM ha854 FULL JOIN hb854 ON ha854.k = hb854.k WHERE ha854.id IS NULL;
SELECT count(*), count(ha854.id), count(hb854.id) FROM ha854 FULL JOIN hb854 ON ha854.k = hb854.k;
RESET work_mem;
RESET hash_mem_multiplier;
RESET enable_mergejoin;
RESET enable_nestloop;
SHOW hash_mem_multiplier;
SELECT name, setting, unit, category, short_desc, context, vartype, min_val, max_val, boot_val FROM pg_settings WHERE name = 'hash_mem_multiplier';
