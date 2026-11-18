-- max_parallel_workers_per_gather is PostgreSQL's setting: default 2, range
-- 0 to 1024, shown by EXPLAIN (SETTINGS) when changed. pos3ql plans no
-- parallel query; these relations are below min_parallel_table_scan_size,
-- where PostgreSQL gives no scan workers, so their plans match at any value.
SHOW max_parallel_workers_per_gather;
SELECT name, setting, unit, category, short_desc, vartype, min_val, max_val, boot_val
  FROM pg_settings WHERE name = 'max_parallel_workers_per_gather';
SET max_parallel_workers_per_gather = 1025;
SET max_parallel_workers_per_gather = -1;
SET max_parallel_workers_per_gather = 0;
SHOW max_parallel_workers_per_gather;
CREATE TABLE p931 (a int, b int);
INSERT INTO p931 SELECT g, g % 10 FROM generate_series(1, 500) g;
ANALYZE p931;
EXPLAIN (SETTINGS, COSTS OFF) SELECT b, count(*) FROM p931 GROUP BY b;
SET max_parallel_workers_per_gather = 4;
EXPLAIN SELECT b, count(*) FROM p931 GROUP BY b;
RESET max_parallel_workers_per_gather;
SHOW max_parallel_workers_per_gather;
SELECT current_setting('max_parallel_workers_per_gather');
DROP TABLE p931;
