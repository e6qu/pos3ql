-- Data-modifying CTEs (WITH x AS (INSERT/UPDATE/DELETE ... RETURNING ...)),
-- matching PostgreSQL 18. Each data-modifying sub-statement runs exactly once;
-- its RETURNING rows become the CTE relation; and — the subtle part — all the
-- WITH sub-statements and the main query share one command snapshot, so the
-- main query does NOT see a CTE's base-table modifications except through its
-- RETURNING relation. pos3ql models this with a per-command MVCC snapshot.
--
-- Distinctive names + drop up front (the differential corpora share a database).
DROP TABLE IF EXISTS dmc_src;
DROP TABLE IF EXISTS dmc_log;

CREATE TABLE dmc_src (id int, v text);
INSERT INTO dmc_src VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd');
CREATE TABLE dmc_log (id int, v text);

-- DELETE ... RETURNING as a relation: the deleted rows are visible through the
-- CTE name and ordered/aggregated like any table.
WITH moved AS (DELETE FROM dmc_src WHERE id <= 2 RETURNING id, v)
SELECT id, v FROM moved ORDER BY id;

-- The command snapshot: the main query reads the base table as it was BEFORE
-- the statement, so the two just-deleted rows are still counted here even
-- though the DELETE has run.
WITH moved AS (DELETE FROM dmc_src WHERE id = 3 RETURNING id)
SELECT (SELECT count(*) FROM moved) AS deleted,
       (SELECT count(*) FROM dmc_src) AS still_visible;

-- Reset for the INSERT/UPDATE cases.
DELETE FROM dmc_src;
INSERT INTO dmc_src VALUES (1, 'a'), (2, 'b'), (3, 'c');

-- INSERT ... RETURNING as a relation.
WITH added AS (INSERT INTO dmc_src VALUES (10, 'x'), (20, 'y') RETURNING id, v)
SELECT id, v FROM added ORDER BY id;

-- UPDATE ... RETURNING as a relation; the main query still sees the pre-update
-- value of a row it reads directly.
WITH bumped AS (UPDATE dmc_src SET v = 'Z' WHERE id = 1 RETURNING id, v)
SELECT (SELECT v FROM bumped) AS updated_to,
       (SELECT v FROM dmc_src WHERE id = 1) AS base_still;

-- A plain (query) CTE and a data-modifying CTE side by side, the query CTE
-- reading the base table under the same snapshot.
WITH del AS (DELETE FROM dmc_src WHERE id = 2 RETURNING id),
     survivors AS (SELECT count(*) AS c FROM dmc_src)
SELECT (SELECT count(*) FROM del) AS deleted, (SELECT c FROM survivors) AS before;

-- The RETURNING relation column rename list applies.
DELETE FROM dmc_src;
INSERT INTO dmc_src VALUES (7, 'g');
WITH r(the_id, the_v) AS (DELETE FROM dmc_src RETURNING id, v)
SELECT the_id, the_v FROM r;

-- Cleanup.
DROP TABLE dmc_src;
DROP TABLE dmc_log;
