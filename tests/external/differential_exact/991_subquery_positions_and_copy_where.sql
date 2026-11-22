-- Parse analysis reports a subquery where no subquery is allowed at the
-- subquery itself; COPY FROM WHERE rejects subqueries, aggregates and window
-- functions where it reaches them, then a predicate other than boolean, then
-- one reading a generated column (a whole-row reference reads them all).
CREATE TABLE sq (a int, b int[], g int GENERATED ALWAYS AS (a * 2) STORED);
CREATE PROCEDURE sq_proc(x int) LANGUAGE plpgsql AS $$ BEGIN END $$;
CALL sq_proc(1 + (SELECT 1));
CALL sq_proc(1 + 2 + EXISTS (SELECT 1)::int);
CREATE TABLE sq2 (a int CHECK (a > (SELECT 1)));
CREATE TABLE sq3 (a int, c int GENERATED ALWAYS AS (a + (SELECT 1)) STORED);
CREATE TABLE sq4 (a int DEFAULT (1 + (SELECT 1)));
PREPARE sq_q(int) AS SELECT $1;
EXECUTE sq_q(1 + (SELECT 1));
CREATE STATISTICS sq_st ON (a + (SELECT 1)), a FROM sq;
CREATE FUNCTION sq_trf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE TRIGGER sq_trg BEFORE INSERT ON sq FOR EACH ROW WHEN (NEW.a > (SELECT 1)) EXECUTE FUNCTION sq_trf();
\copy sq (a) FROM '/dev/null' WHERE b[(SELECT 1)] > 0
\copy sq (a) FROM '/dev/null' WHERE g > 0
\copy sq (a) FROM '/dev/null' WHERE a > 0 AND EXISTS (SELECT 1)
\copy sq (a) FROM '/dev/null' WHERE a + 1
\copy sq (a) FROM '/dev/null' WHERE g + 1
\copy sq (a) FROM '/dev/null' WHERE b[sum(a)] > 0
\copy sq (a) FROM '/dev/null' WHERE a > sum(a) OVER ()
\copy sq (a) FROM '/dev/null' WHERE b[1:(SELECT 1)] IS NULL
\copy sq (a) FROM '/dev/null' WHERE a IN (1, (SELECT 1))
\copy sq (a) FROM '/dev/null' WHERE g > (SELECT 1)
\copy sq (a) FROM '/dev/null' WHERE sq IS NULL
\copy sq (a) FROM '/dev/null' WHERE nosuch > 0 AND (SELECT true)
\copy sq (a) FROM '/dev/null' WHERE a > 0
