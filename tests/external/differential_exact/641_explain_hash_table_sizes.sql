-- EXPLAIN ANALYZE reports a hash join's table as PostgreSQL sizes it: at
-- least 1024 buckets, more for a larger row estimate, doubled while the rows
-- outnumber them, and memory counting each row's minimal tuple and the
-- bucket pointers. The rows stay under the forced-spill run's 256 KiB
-- memtable.
SET timezone = 'UTC';
CREATE TABLE hs_build (id int, small text, big text, n numeric, d date, ts timestamptz, f float8, b bool, i8 bigint);
CREATE TABLE hs_probe (id int, note text);
INSERT INTO hs_build SELECT g, 'v' || g, CASE WHEN g % 10 = 0 THEN repeat('x', 100 + g % 60) ELSE 'w' || g END, g * 1.5, DATE '2024-01-01' + g, TIMESTAMPTZ '2024-01-01 00:00+00' + g * INTERVAL '1 hour', g / 3.0, g % 2 = 0, g * 1000000000::bigint FROM generate_series(1, 1500) g;
UPDATE hs_build SET small = NULL WHERE id % 7 = 0;
INSERT INTO hs_probe SELECT g, 'q' || g FROM generate_series(1, 2000) g;
ANALYZE hs_build;
ANALYZE hs_probe;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT hs_probe.note, hs_build.small FROM hs_probe JOIN hs_build ON hs_build.id = hs_probe.id;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT hs_probe.note, hs_build.big, hs_build.n FROM hs_probe JOIN hs_build ON hs_build.id = hs_probe.id;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT hs_probe.note, hs_build.f, hs_build.b, hs_build.i8, hs_build.small FROM hs_probe JOIN hs_build ON hs_build.id = hs_probe.id;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT hs_probe.note FROM hs_probe JOIN hs_build ON hs_build.id = hs_probe.id WHERE hs_build.id < 300;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT hs_probe.note, hs_build.b, hs_build.small, hs_build.tableoid FROM hs_probe JOIN hs_build ON hs_build.id = hs_probe.id;
EXPLAIN (ANALYZE, FORMAT JSON, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT hs_probe.note, hs_build.d, hs_build.ts FROM hs_probe JOIN hs_build ON hs_build.id = hs_probe.id;
EXPLAIN (ANALYZE, FORMAT YAML, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT hs_probe.note, hs_build.i8 FROM hs_probe JOIN hs_build ON hs_build.id = hs_probe.id;
CREATE TABLE hs_tiny (id int, label varchar(10), price numeric(40,2), at time);
INSERT INTO hs_tiny VALUES (1, 'one', 1.25, '01:00'), (2, NULL, NULL, NULL), (3, 'three', 'NaN', '03:00'), (4, 'four', 123456789012345678901234567890.12, '04:00');
ANALYZE hs_tiny;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT hs_probe.note, hs_tiny.* FROM hs_probe JOIN hs_tiny ON hs_tiny.id = hs_probe.id;
DROP TABLE hs_build, hs_probe, hs_tiny;
