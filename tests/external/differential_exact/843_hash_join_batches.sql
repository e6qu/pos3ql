-- A hash join whose inner rows outgrow work_mem: ExecChooseHashTableSize
-- plans several batches, each row goes to the batch its hash value names
-- (hashint4, hashint8, hashtext), the outer key's most common values get
-- skew buckets, and a batch that outgrows its memory doubles the batches
-- (ExecHashIncreaseNumBatches); rows come back batch by batch, as
-- PostgreSQL returns them.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeHash.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeHashjoin.c
CREATE TABLE p843 (k int, b bigint, t text, v int);
CREATE TABLE q843 (k int, b bigint, w int);
INSERT INTO p843 SELECT g % 400, g % 300, repeat('p', 100) || (g % 250), g FROM generate_series(1, 1000) g;
INSERT INTO q843 SELECT (g * 7) % 400, (g * 3) % 300, g FROM generate_series(1, 1500) g;
INSERT INTO q843 SELECT 5, 5, g FROM generate_series(1, 300) g;
ANALYZE p843; ANALYZE q843;
SET work_mem = '64kB';
EXPLAIN SELECT p843.t, q843.w FROM p843 JOIN q843 ON p843.k = q843.k;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT p843.t, q843.w FROM p843 JOIN q843 ON p843.k = q843.k;
SELECT p843.v, q843.w, p843.t FROM p843 JOIN q843 ON p843.k = q843.k WHERE q843.w < 30;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT p843.t, q843.w FROM p843 JOIN q843 ON p843.b = q843.b;
SELECT p843.v, q843.w FROM p843 JOIN q843 ON p843.b = q843.b WHERE q843.w < 40 AND p843.t <> '';
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT p843.t, q843.w FROM p843 JOIN q843 ON p843.k = q843.k AND p843.b = q843.b;
SELECT p843.v, q843.w FROM p843 JOIN q843 ON p843.k = q843.k AND p843.b = q843.b WHERE p843.t <> '';
-- Planned as one batch, the table outgrows its memory and splits.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT p843.t, q843.w FROM q843 JOIN p843 ON p843.k = q843.k WHERE p843.v % 7 < 9;
SELECT p843.v, q843.w FROM q843 JOIN p843 ON p843.k = q843.k WHERE p843.v % 7 < 9 AND p843.t <> '' AND q843.w < 25;
RESET work_mem;
