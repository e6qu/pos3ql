-- PostgreSQL 18 cumulative database, table and index statistics.
\pset null 'NULL'

DROP TABLE IF EXISTS cumulative_statistics_probe;
CREATE TABLE cumulative_statistics_probe (
    id integer PRIMARY KEY,
    value integer
);
CREATE INDEX cumulative_statistics_probe_value_idx
    ON cumulative_statistics_probe(value);
INSERT INTO cumulative_statistics_probe
SELECT value, value * 10 FROM generate_series(1, 100) AS value;
SELECT pg_stat_force_next_flush();
SELECT pg_stat_reset();

BEGIN;
SELECT value FROM cumulative_statistics_probe WHERE id = 50;
UPDATE cumulative_statistics_probe SET value = 501 WHERE id = 50;
DELETE FROM cumulative_statistics_probe WHERE id = 51;
INSERT INTO cumulative_statistics_probe VALUES (101, 1010);
SELECT seq_scan + idx_scan > 0,
       seq_tup_read + idx_tup_fetch > 0,
       n_tup_ins = 1, n_tup_upd = 1, n_tup_del = 1,
       n_tup_hot_upd = 0, n_tup_newpage_upd = 0
  FROM pg_stat_xact_user_tables
 WHERE relname = 'cumulative_statistics_probe';
ROLLBACK;
SELECT pg_stat_force_next_flush();

SELECT pg_typeof(relid), pg_typeof(schemaname), pg_typeof(seq_scan),
       pg_typeof(last_seq_scan), pg_typeof(total_analyze_time),
       seq_scan + idx_scan > 0, seq_tup_read + idx_tup_fetch > 0,
       n_tup_ins >= 1, n_tup_upd >= 1, n_tup_del >= 1,
       n_dead_tup >= 1, n_ins_since_vacuum >= 1
  FROM pg_stat_user_tables
 WHERE relname = 'cumulative_statistics_probe';

SELECT pg_typeof(indexrelid), pg_typeof(idx_scan), pg_typeof(last_idx_scan),
       idx_scan > 0, idx_tup_read > 0, idx_tup_fetch > 0
  FROM pg_stat_user_indexes
 WHERE indexrelname = 'cumulative_statistics_probe_pkey';

SELECT count(*) = 1 FROM pg_stat_all_tables
 WHERE relname = 'cumulative_statistics_probe';
SELECT count(*) = 1 FROM pg_stat_user_tables
 WHERE relname = 'cumulative_statistics_probe';
SELECT count(*) = 0 FROM pg_stat_sys_tables
 WHERE relname = 'cumulative_statistics_probe';

ANALYZE cumulative_statistics_probe;
SELECT n_live_tup = 100, n_mod_since_analyze = 0,
       analyze_count > 0, last_analyze IS NOT NULL,
       total_analyze_time >= 0
  FROM pg_stat_user_tables
 WHERE relname = 'cumulative_statistics_probe';
VACUUM cumulative_statistics_probe;
SELECT n_dead_tup = 0, n_ins_since_vacuum = 0,
       vacuum_count > 0, last_vacuum IS NOT NULL,
       total_vacuum_time >= 0
  FROM pg_stat_user_tables
 WHERE relname = 'cumulative_statistics_probe';

SELECT pg_stat_reset_single_table_counters(
    'cumulative_statistics_probe'::regclass);
SELECT seq_scan = 0, n_tup_ins = 0, n_tup_upd = 0, n_tup_del = 0,
       idx_scan > 0
  FROM pg_stat_user_tables
 WHERE relname = 'cumulative_statistics_probe';
SELECT pg_stat_reset_single_table_counters(
    'cumulative_statistics_probe_pkey'::regclass);
SELECT idx_scan = 0, idx_tup_read = 0, idx_tup_fetch = 0
  FROM pg_stat_user_indexes
 WHERE indexrelname = 'cumulative_statistics_probe_pkey';

BEGIN;
INSERT INTO cumulative_statistics_probe VALUES (200, 2000);
TRUNCATE cumulative_statistics_probe;
INSERT INTO cumulative_statistics_probe VALUES (201, 2010);
COMMIT;
SELECT pg_stat_force_next_flush();
SELECT count(*) = 1 FROM cumulative_statistics_probe;
SELECT n_tup_ins = 1, n_tup_upd = 0, n_tup_del = 0,
       n_live_tup = 1, n_dead_tup = 0,
       n_mod_since_analyze = 1, n_ins_since_vacuum = 1
  FROM pg_stat_user_tables
 WHERE relname = 'cumulative_statistics_probe';

SELECT pg_stat_reset_single_table_counters(
    'cumulative_statistics_probe'::regclass);
BEGIN;
INSERT INTO cumulative_statistics_probe VALUES (202, 2020);
TRUNCATE cumulative_statistics_probe;
INSERT INTO cumulative_statistics_probe VALUES (203, 2030), (204, 2040);
ROLLBACK;
SELECT pg_stat_force_next_flush();
SELECT count(*) = 1 FROM cumulative_statistics_probe;
SELECT n_tup_ins = 1, n_tup_upd = 0, n_tup_del = 0,
       n_live_tup = 0, n_dead_tup = 1,
       n_mod_since_analyze = 0, n_ins_since_vacuum = 1
  FROM pg_stat_user_tables
 WHERE relname = 'cumulative_statistics_probe';

CREATE TABLE cumulative_statistics_savepoint_probe(id integer PRIMARY KEY);
INSERT INTO cumulative_statistics_savepoint_probe VALUES (1);
SELECT pg_stat_force_next_flush();
SELECT pg_stat_reset_single_table_counters(
    'cumulative_statistics_savepoint_probe'::regclass);
BEGIN;
INSERT INTO cumulative_statistics_savepoint_probe VALUES (2);
SAVEPOINT outer_statistics;
TRUNCATE cumulative_statistics_savepoint_probe;
INSERT INTO cumulative_statistics_savepoint_probe VALUES (3);
SAVEPOINT inner_statistics;
TRUNCATE cumulative_statistics_savepoint_probe;
INSERT INTO cumulative_statistics_savepoint_probe VALUES (4);
ROLLBACK TO inner_statistics;
INSERT INTO cumulative_statistics_savepoint_probe VALUES (5);
ROLLBACK TO outer_statistics;
INSERT INTO cumulative_statistics_savepoint_probe VALUES (6);
COMMIT;
SELECT pg_stat_force_next_flush();
SELECT array_agg(id ORDER BY id) FROM cumulative_statistics_savepoint_probe;
SELECT n_tup_ins = 3, n_tup_upd = 0, n_tup_del = 0,
       n_live_tup = 2, n_dead_tup = 1,
       n_mod_since_analyze = 2, n_ins_since_vacuum = 3
  FROM pg_stat_user_tables
 WHERE relname = 'cumulative_statistics_savepoint_probe';
DROP TABLE cumulative_statistics_savepoint_probe;

SELECT pg_typeof(datid), pg_typeof(numbackends), pg_typeof(xact_commit),
       pg_typeof(session_time), xact_commit > 0, xact_rollback > 0,
       tup_inserted > 0, tup_updated > 0, tup_deleted > 0,
       stats_reset IS NOT NULL
  FROM pg_stat_database
 WHERE datname = current_database();
SELECT count(*) = 1
  FROM pg_stat_database WHERE datid = 0 AND datname IS NULL;

SELECT count(*) = 10
  FROM pg_class
 WHERE oid IN (12146,12151,12156,12161,12165,12170,12187,12192,12196,12270);
SELECT count(*) = 183
  FROM pg_attribute
 WHERE attrelid IN (12146,12151,12156,12161,12165,12170,12187,12192,12196,12270)
   AND attnum > 0 AND NOT attisdropped;
SELECT oid, pronargs, prorettype, provolatile, proparallel, proisstrict,
       proacl IS NULL
  FROM pg_proc WHERE oid IN (2137,2230,2274,3776) ORDER BY oid;

SELECT pg_stat_clear_snapshot(), pg_stat_force_next_flush();
DROP TABLE cumulative_statistics_probe;
