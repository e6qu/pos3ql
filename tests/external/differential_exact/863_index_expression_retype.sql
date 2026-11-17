-- ALTER COLUMN TYPE rebuilds an index whose expression calls a function with
-- the old column type: the call no longer resolves, as when the index is
-- parsed again, and the statement fails with PostgreSQL's error.
CREATE FUNCTION rw863(integer) RETURNS integer IMMUTABLE LANGUAGE plpgsql
  AS $$BEGIN RETURN $1 + 1; END$$;
CREATE FUNCTION sq863(integer) RETURNS integer IMMUTABLE LANGUAGE sql
  AS $$SELECT $1 * 2$$;
CREATE TABLE t863 (a integer, b integer);
INSERT INTO t863 SELECT g, g FROM generate_series(1, 5) g;
CREATE INDEX t863_rw ON t863 (rw863(a));
ALTER TABLE t863 ALTER COLUMN a TYPE bigint;
SELECT attname, atttypid::regtype FROM pg_attribute
  WHERE attrelid = 't863'::regclass AND attnum > 0 ORDER BY attnum;
DROP INDEX t863_rw;
CREATE INDEX t863_sq ON t863 (sq863(b));
ALTER TABLE t863 ALTER COLUMN b TYPE bigint;
CREATE INDEX t863_rwp ON t863 (b) WHERE rw863(b) > 2;
ALTER TABLE t863 ALTER COLUMN b TYPE numeric;
ALTER TABLE t863 ALTER COLUMN a TYPE numeric;
SELECT count(*) FROM t863;
