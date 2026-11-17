-- An index build computes each row's key once: a PL/pgSQL function in an
-- index expression or predicate raises its notice once per row and index,
-- in the order PostgreSQL's build reads the rows, also for a unique build,
-- a nested call and a rebuild of every index of a table.
CREATE FUNCTION ib833_twice(integer) RETURNS integer IMMUTABLE LANGUAGE plpgsql
  AS $$BEGIN RAISE NOTICE 'twice %', $1; RETURN $1 * 2; END$$;
CREATE FUNCTION ib833_next(integer) RETURNS integer IMMUTABLE LANGUAGE plpgsql
  AS $$BEGIN RAISE NOTICE 'next %', $1; RETURN $1 + 1; END$$;
CREATE TABLE ib833 (a integer, b integer);
INSERT INTO ib833 SELECT g, g % 2 FROM generate_series(1, 6) g;
CREATE INDEX ib833_twice ON ib833 (ib833_twice(a));
CREATE UNIQUE INDEX ib833_unique ON ib833 (ib833_twice(a));
CREATE INDEX ib833_nested ON ib833 (ib833_twice(ib833_next(a)));
CREATE INDEX ib833_partial ON ib833 (a) WHERE ib833_twice(a) > 6;
CREATE UNIQUE INDEX ib833_duplicate ON ib833 (ib833_twice(b));
REINDEX TABLE ib833;
SELECT a FROM ib833 WHERE ib833_twice(a) = 8;
