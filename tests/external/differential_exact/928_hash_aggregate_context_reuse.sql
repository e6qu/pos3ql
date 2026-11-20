-- PostgreSQL's aset.c keeps freed memory contexts of the default sizes on
-- a per-backend freelist and reuses them without recomputing their chunk
-- limit. A hash aggregate built under work_mem below 1MB returns its work
-- context, whose chunk limit is lower, to that list; a later hash
-- aggregate's meta context may reuse it, so its bucket arrays are counted
-- differently and its spill point and row order change. pos3ql does not
-- track which context the freelist hands out: after such an aggregate, a
-- later hash aggregate whose bucket array outgrows the lower limit fails
-- with 0A000 (src/sql/tests/hash_aggregate_context_reuse.rs). Each
-- session here builds no such aggregate first: a plain query, a filter, a
-- sorted grouping, a hash join, a sort, or a hash aggregate explained under
-- 4MB or 1MB leave the later 64kB aggregate as in a new session.
\set origin :DBNAME
CREATE TABLE h928 (id int, a int, b text, c bigint) WITH (autovacuum_enabled = off);
INSERT INTO h928 SELECT i, i % 20, 'k' || (i % 5), i % 20 FROM generate_series(1, 200) i;
ANALYZE h928;
INSERT INTO h928
SELECT i, (i * 7919) % 5000, 'k' || (i % 2500), (i * 31) % 40000
FROM generate_series(201, 2200) i;
INSERT INTO h928
SELECT i, (i * 7919) % 5000, 'k' || (i % 2500), (i * 31) % 40000
FROM generate_series(2201, 4200) i;
INSERT INTO h928
SELECT i, (i * 7919) % 5000, 'k' || (i % 2500), (i * 31) % 40000
FROM generate_series(4201, 6200) i;
\c :origin
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT a FROM h928;
\c :origin
SELECT 1;
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT a FROM h928;
\c :origin
SELECT count(*) FROM h928 WHERE a > 3;
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT a FROM h928;
\c :origin
SELECT a FROM h928 GROUP BY a ORDER BY a LIMIT 1;
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT a FROM h928;
\c :origin
SELECT count(*) FROM h928 x JOIN h928 y USING (id);
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT a FROM h928;
\c :origin
SELECT * FROM h928 ORDER BY b, id LIMIT 1;
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT a FROM h928;
\c :origin
SET work_mem = '4MB';
EXPLAIN (COSTS OFF) SELECT DISTINCT a FROM h928;
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT a FROM h928;
\c :origin
SET work_mem = '1MB';
EXPLAIN (COSTS OFF) SELECT DISTINCT a FROM h928;
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT DISTINCT a FROM h928;
\c :origin
DROP TABLE h928;
