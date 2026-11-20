-- A DISTINCT or GROUP BY PostgreSQL plans as a HashAggregate from
-- statistics that predate the rows now in the table outgrows its table: it
-- spills the rows of new groups to partitions and reads them back batch by
-- batch, last partition first, so its rows follow the batches. A batch that
-- does not fit spills again by the next hash bits.
-- autovacuum_enabled = off keeps autovacuum from refreshing the statistics
-- the plan is chosen from; the rows commit in small statements. Each
-- aggregate runs in a session of its own: PostgreSQL reuses an earlier
-- small-work_mem aggregate's memory context, which pos3ql does not model
-- (src/sql/tests/hash_aggregate_context_reuse.rs).
\set origin :DBNAME
CREATE TABLE h926 (id int, a int, b text, c bigint) WITH (autovacuum_enabled = off);
INSERT INTO h926 SELECT i, i % 20, 'k' || (i % 5), i % 20 FROM generate_series(1, 200) i;
ANALYZE h926;
INSERT INTO h926
SELECT i, (i * 7919) % 5000, 'k' || (i % 2500), (i * 31) % 40000
FROM generate_series(201, 1200) i;
INSERT INTO h926
SELECT i, (i * 7919) % 5000, 'k' || (i % 2500), (i * 31) % 40000
FROM generate_series(1201, 2200) i;
INSERT INTO h926
SELECT i, (i * 7919) % 5000, 'k' || (i % 2500), (i * 31) % 40000
FROM generate_series(2201, 3200) i;
INSERT INTO h926
SELECT i, (i * 7919) % 5000, 'k' || (i % 2500), (i * 31) % 40000
FROM generate_series(3201, 4200) i;
INSERT INTO h926
SELECT i, (i * 7919) % 5000, 'k' || (i % 2500), (i * 31) % 40000
FROM generate_series(4201, 5200) i;
INSERT INTO h926
SELECT i, (i * 7919) % 5000, 'k' || (i % 2500), (i * 31) % 40000
FROM generate_series(5201, 6200) i;
INSERT INTO h926
SELECT i, (i * 7919) % 5000, 'k' || (i % 2500), (i * 31) % 40000
FROM generate_series(6201, 7200) i;
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT a FROM h926;
\c :origin
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT b FROM h926;
\c :origin
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT a, b FROM h926;
\c :origin
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT c FROM h926;
\c :origin
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT a FROM h926 OFFSET 4000;
\c :origin
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT c FROM h926 OFFSET 100 LIMIT 25;
\c :origin
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT DISTINCT c FROM h926;
\c :origin
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT DISTINCT a, b FROM h926;
\c :origin
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT a, count(*) FROM h926 GROUP BY a;
\c :origin
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a, count(*) FROM h926 GROUP BY a;
\c :origin
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT a, sum(c) FROM h926 GROUP BY a LIMIT 40;
\c :origin
SET work_mem = '128kB';
SET hash_mem_multiplier = 1;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT DISTINCT c FROM h926;
\c :origin
SET work_mem = '256kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT c FROM h926;
\c :origin
SET work_mem = '256kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT a, b FROM h926 OFFSET 2000;
\c :origin
SET work_mem = '256kB';
SET hash_mem_multiplier = 1;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT DISTINCT c FROM h926;
\c :origin
SET work_mem = '1MB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT c FROM h926;
\c :origin
SET work_mem = '1MB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT a, b FROM h926;
\c :origin
SET work_mem = '1MB';
SET hash_mem_multiplier = 1;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT DISTINCT c FROM h926;
\c :origin
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT a FROM h926 WHERE id % 3 = 0;
\c :origin
SET work_mem = '4MB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT a FROM h926 OFFSET 4990;
\c :origin
SET work_mem = '4MB';
SET hash_mem_multiplier = 1;
EXPLAIN (COSTS OFF) SELECT DISTINCT a FROM h926;
DROP TABLE h926;
