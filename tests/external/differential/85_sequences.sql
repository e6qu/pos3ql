-- Sequence definition and restart semantics, matched against PostgreSQL 18.4.
DROP SEQUENCE IF EXISTS staged_sequence_diff;

CREATE SEQUENCE staged_sequence_diff START WITH 10 INCREMENT BY 2;
SELECT nextval('staged_sequence_diff');

BEGIN;
ALTER SEQUENCE staged_sequence_diff INCREMENT BY 7 RESTART WITH 40;
SELECT increment_by, last_value FROM pg_sequences WHERE sequencename = 'staged_sequence_diff';
SAVEPOINT preserved_definition;
SELECT nextval('staged_sequence_diff');
ALTER SEQUENCE staged_sequence_diff INCREMENT BY 11 RESTART WITH 80;
ROLLBACK TO SAVEPOINT preserved_definition;
SELECT nextval('staged_sequence_diff');
COMMIT;

SELECT increment_by, last_value FROM pg_sequences WHERE sequencename = 'staged_sequence_diff';
SELECT nextval('staged_sequence_diff');
DROP SEQUENCE staged_sequence_diff;

CREATE SEQUENCE cached_sequence_diff MINVALUE 1 MAXVALUE 9 CACHE 5;
SELECT nextval('cached_sequence_diff');
SELECT last_value, log_cnt, is_called FROM cached_sequence_diff;
SELECT nextval('cached_sequence_diff');
SELECT last_value, log_cnt, is_called FROM cached_sequence_diff;
ALTER SEQUENCE cached_sequence_diff INCREMENT BY 2;
SELECT last_value, log_cnt, is_called FROM cached_sequence_diff;
SELECT nextval('cached_sequence_diff');
SELECT last_value, log_cnt, is_called FROM cached_sequence_diff;
DROP SEQUENCE cached_sequence_diff;

CREATE SEQUENCE cycling_cache_diff MINVALUE 1 MAXVALUE 3 CACHE 5 CYCLE;
SELECT nextval('cycling_cache_diff') FROM generate_series(1, 5);
SELECT last_value, log_cnt, is_called FROM cycling_cache_diff;
DROP SEQUENCE cycling_cache_diff;
