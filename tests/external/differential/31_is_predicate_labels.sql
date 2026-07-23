-- `IS TRUE` / `IS FALSE` / `IS DISTINCT FROM` desugar to CASE internally, but
-- PostgreSQL labels their output column `?column?`, not `case` — a CASE the
-- query actually wrote is labelled `case`. The values are unchanged.

SELECT 1 IS DISTINCT FROM 2;
SELECT 1 IS DISTINCT FROM 1;
SELECT NULL IS DISTINCT FROM 1;
SELECT NULL IS NOT DISTINCT FROM NULL;
SELECT NULL IS NOT DISTINCT FROM 1;
SELECT true IS TRUE;
SELECT true IS FALSE;
SELECT true IS NOT TRUE;
SELECT false IS NOT FALSE;
SELECT NULL IS TRUE;
SELECT 1 IS NULL;
SELECT 1 IS NOT NULL;

-- a real CASE keeps the `case` label
SELECT CASE WHEN true THEN 1 END;
SELECT CASE WHEN true THEN 1 ELSE 2 END;
SELECT CASE WHEN false THEN 1 ELSE 2 END;

-- the predicates compose and still evaluate correctly
CREATE TABLE ispred (a int);
INSERT INTO ispred VALUES (1), (2), (NULL);
SELECT a, a IS DISTINCT FROM 1 FROM ispred ORDER BY a;
SELECT a FROM ispred WHERE a IS NOT DISTINCT FROM 2;
SELECT count(*) FROM ispred WHERE a IS NULL;
DROP TABLE ispred;
