-- ANALYZE's options as ExecVacuum checks them: VERBOSE and SKIP_LOCKED take
-- Booleans, BUFFER_USAGE_LIMIT a size of 0 or 128 kB through 16 GB, and any
-- other option is unrecognized. SKIP_LOCKED passes over a named table whose
-- lock another transaction (here a prepared one) holds, with a warning.
CREATE TABLE ao (a int, b text);
CREATE TABLE ao_parent (a int) PARTITION BY RANGE (a);
CREATE TABLE ao_child PARTITION OF ao_parent FOR VALUES FROM (0) TO (100);
INSERT INTO ao SELECT g, 'v' || g FROM generate_series(1, 50) g;
ANALYZE (SKIP_LOCKED) ao;
ANALYZE (skip_locked 'on', verbose off, skip_locked false) ao;
ANALYZE (verbose false, buffer_usage_limit 0) ao;
ANALYZE (buffer_usage_limit 128) ao (a);
ANALYZE (buffer_usage_limit '1MB') ao;
ANALYZE (buffer_usage_limit '128 kB') ao;
ANALYZE (buffer_usage_limit '16GB') ao;
ANALYZE (buffer_usage_limit +128) ao;
ANALYZE (buffer_usage_limit 127.6) ao;
ANALYZE (buffer_usage_limit '1kB') ao;
ANALYZE (buffer_usage_limit '17GB') ao;
ANALYZE (buffer_usage_limit 'abc') ao;
ANALYZE (buffer_usage_limit '10 xx') ao;
ANALYZE (buffer_usage_limit 99999999999) ao;
ANALYZE (buffer_usage_limit -1) ao;
ANALYZE (buffer_usage_limit true) ao;
ANALYZE (buffer_usage_limit) ao;
ANALYZE (verbose 2) ao;
ANALYZE (verbose 1.0) ao;
ANALYZE (verbose x) ao;
ANALYZE (skip_locked 'maybe') ao;
ANALYZE (foo) ao;
ANALYZE ("VERBOSE") ao;
ANALYZE (full) ao;
ANALYZE (analyse) ao;
ANALYZE (format) ao;
ANALYZE (process_main) ao;
ANALYZE (disable_page_skipping) ao;
ANALYZE (skip_locked, foo) missing_table;
ANALYZE (skip_locked) missing_table;
ANALYZE (verbose 2, foo) ao;
ANALYZE (select) ao;
ANALYZE (verbose = true) ao;
ANALYZE ();
ANALYZE VERBOSE FULL;
ANALYZE skip_locked;
BEGIN;
LOCK TABLE ao IN ACCESS EXCLUSIVE MODE;
LOCK TABLE ao_child IN SHARE UPDATE EXCLUSIVE MODE;
PREPARE TRANSACTION 'analyze_options_lock';
ANALYZE (SKIP_LOCKED) ao;
ANALYZE (SKIP_LOCKED) ao_child, ao;
ANALYZE (SKIP_LOCKED) ao_parent;
ANALYZE (SKIP_LOCKED, BUFFER_USAGE_LIMIT '256kB') ao (b), ao_parent;
ANALYZE (SKIP_LOCKED);
COMMIT PREPARED 'analyze_options_lock';
ANALYZE (SKIP_LOCKED) ao, ao_child;
SELECT relname, reltuples FROM pg_class
  WHERE relname IN ('ao', 'ao_child', 'ao_parent') ORDER BY relname;
DROP TABLE ao, ao_parent;
