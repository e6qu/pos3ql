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

-- The main statement may be INSERT/UPDATE/DELETE/MERGE, not only SELECT.
-- Ordinary and recursive query CTEs bind in every DML source position.
INSERT INTO dmc_src VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd');
WITH picked AS (SELECT id, v FROM dmc_src WHERE id <= 2 ORDER BY id)
INSERT INTO dmc_log SELECT id, v FROM picked RETURNING id, v;

WITH picked AS (SELECT 3 AS id, 'C' AS v)
UPDATE dmc_src SET v = picked.v FROM picked
WHERE dmc_src.id = picked.id RETURNING dmc_src.id, dmc_src.v;

CREATE TABLE dmc_copy_source (id int, payload text);
CREATE TABLE dmc_copy_target (id int, payload text);
INSERT INTO dmc_copy_source VALUES (1, 'copied'), (2, 'ignored');
INSERT INTO dmc_copy_target VALUES (1, 'before');
UPDATE dmc_copy_target AS target SET payload = source.payload
FROM dmc_copy_source AS source WHERE target.id = source.id;
SELECT id, payload FROM dmc_copy_target;

CREATE SEQUENCE dmc_update_sequence;
UPDATE dmc_copy_target AS target SET payload = nextval('dmc_update_sequence')::text
FROM dmc_copy_source AS source WHERE target.id = source.id;
SELECT id, payload FROM dmc_copy_target;
SELECT nextval('dmc_update_sequence');

WITH picked AS (SELECT 4 AS id)
DELETE FROM dmc_src USING picked
WHERE dmc_src.id = picked.id RETURNING dmc_src.id;

WITH RECURSIVE numbers(n) AS (
    VALUES (10) UNION ALL SELECT n + 1 FROM numbers WHERE n < 12
)
INSERT INTO dmc_log SELECT n, 'recursive' FROM numbers RETURNING id, v;

WITH incoming AS (
    SELECT 1 AS id, 'A' AS v UNION ALL SELECT 5, 'e'
)
MERGE INTO dmc_src AS target USING incoming AS source
ON target.id = source.id
WHEN MATCHED THEN UPDATE SET v = source.v
WHEN NOT MATCHED THEN INSERT (id, v) VALUES (source.id, source.v);
SELECT id, v FROM dmc_src ORDER BY id;

-- Earlier query and data-modifying CTEs feed later data-modifying CTEs, whose
-- materialized RETURNING rows in turn feed the main DML statement. The main
-- statement does not see the CTE writes through the base tables (one command
-- snapshot); it sees them only through the named RETURNING relations.
CREATE TABLE dmc_final (id int, v text);
WITH wanted AS (SELECT id FROM dmc_src WHERE id IN (2,3)),
     moved AS (
         DELETE FROM dmc_src USING wanted
         WHERE dmc_src.id = wanted.id
         RETURNING dmc_src.id, dmc_src.v
     ),
     archived AS (
         INSERT INTO dmc_log
         SELECT id + 100, v FROM moved
         RETURNING id, v
     )
INSERT INTO dmc_final SELECT id, v FROM archived;
SELECT id, v FROM dmc_final ORDER BY id;

-- Cleanup.
DROP TABLE dmc_src;
DROP TABLE dmc_log;
DROP TABLE dmc_final;
DROP TABLE dmc_copy_source;
DROP TABLE dmc_copy_target;
DROP SEQUENCE dmc_update_sequence;
