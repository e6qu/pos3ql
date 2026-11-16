-- An immutable PL/pgSQL function evaluates in index builds and index
-- expressions as PostgreSQL's executor runs it: the statement that builds
-- or maintains the index calls the function for each row, under the
-- maintenance operation's search path pg_catalog, pg_temp when it builds.
CREATE DOMAIN posint755 AS int CHECK (VALUE > 0);
CREATE TABLE lookup755 (k int);
CREATE FUNCTION twice755(int) RETURNS int IMMUTABLE LANGUAGE plpgsql
  AS $$BEGIN RETURN $1 * 2; END$$;
CREATE FUNCTION pos755(int) RETURNS int IMMUTABLE LANGUAGE plpgsql
  AS $$BEGIN RETURN $1::posint755; END$$;
CREATE FUNCTION qpos755(int) RETURNS int IMMUTABLE LANGUAGE plpgsql
  AS $$BEGIN RETURN $1::public.posint755; END$$;
CREATE FUNCTION count755(int) RETURNS bigint IMMUTABLE LANGUAGE plpgsql
  AS $$BEGIN RETURN (SELECT count(*) FROM lookup755); END$$;
CREATE FUNCTION path755(int) RETURNS text IMMUTABLE LANGUAGE plpgsql
  AS $$BEGIN RETURN current_setting('search_path'); END$$;
CREATE TABLE t755 (a int, b int);
INSERT INTO t755 SELECT g, g % 3 FROM generate_series(1, 40) g;
CREATE INDEX t755_twice ON t755 (twice755(a));
CREATE UNIQUE INDEX t755_twice_u ON t755 (twice755(a));
CREATE UNIQUE INDEX t755_b ON t755 (twice755(b));
SELECT a FROM t755 WHERE twice755(a) = 14;
INSERT INTO t755 VALUES (7, 1);
INSERT INTO t755 VALUES (41, 1) RETURNING twice755(a);
UPDATE t755 SET a = 42 WHERE a = 41;
-- Unqualified user objects are not found in the function body.
CREATE INDEX t755_pos ON t755 (pos755(a));
CREATE INDEX t755_count ON t755 (count755(a));
CREATE INDEX t755_qpos ON t755 (qpos755(a - 5));
CREATE INDEX t755_qpos ON t755 (qpos755(a));
INSERT INTO t755 VALUES (-1, 2);
REINDEX TABLE t755;
ANALYZE t755;
SELECT attname, n_distinct FROM pg_stats WHERE tablename = 't755_twice' ORDER BY 1;
-- Materialized views and CREATE TABLE AS run a PL/pgSQL call once per row.
CREATE MATERIALIZED VIEW m755 AS SELECT DISTINCT path755(a) AS path FROM t755;
SELECT * FROM m755;
CREATE TABLE c755 AS SELECT a, twice755(a) AS d FROM t755 WHERE a < 4 ORDER BY a;
SELECT * FROM c755 ORDER BY a;
REFRESH MATERIALIZED VIEW m755;
SELECT * FROM m755;
-- A table rewrite evaluates under the session's path; its rebuilt indexes
-- under the restricted one.
ALTER TABLE c755 ALTER COLUMN d TYPE bigint USING twice755(d);
ALTER TABLE c755 ADD CONSTRAINT c755_check CHECK (twice755(a) < 7);
ALTER TABLE c755 ADD CONSTRAINT c755_check CHECK (twice755(a) < 70);
SELECT * FROM c755 ORDER BY a;
CREATE INDEX c755_pos ON c755 (pos755(a));
CLUSTER t755 USING t755_twice;
VACUUM FULL t755;
