-- Multi-column DML assignments use one typed row source and update targets
-- simultaneously across UPDATE, conflict updates, and MERGE.
DROP TABLE IF EXISTS dml_row_assignment_source;
DROP TABLE IF EXISTS dml_row_assignment;

CREATE TABLE dml_row_assignment (
  id integer PRIMARY KEY,
  a integer DEFAULT 7,
  b integer DEFAULT 8
);
INSERT INTO dml_row_assignment VALUES (1, 10, 20), (2, 30, 40);

UPDATE dml_row_assignment SET (a, b) = (b, a) WHERE id = 1;
UPDATE dml_row_assignment SET (a, b) = ROW(DEFAULT, DEFAULT) WHERE id = 2;
UPDATE dml_row_assignment SET (a, b) = (SELECT 50, 60) WHERE id = 1;
INSERT INTO dml_row_assignment VALUES (1, 0, 0)
  ON CONFLICT (id) DO UPDATE SET (a, b) = (excluded.b, excluded.a);

CREATE TABLE dml_row_assignment_source (id integer, a integer, b integer);
INSERT INTO dml_row_assignment_source VALUES (2, 70, 80);
MERGE INTO dml_row_assignment AS target USING dml_row_assignment_source AS source
  ON target.id = source.id
  WHEN MATCHED THEN UPDATE SET (a, b) = (source.b, source.a);
SELECT id, a, b FROM dml_row_assignment ORDER BY id;

-- Arity, duplicate targets, and a multi-row scalar source fail rather than
-- being truncated, applied sequentially, or silently choosing a row.
UPDATE dml_row_assignment SET (a, b) = (1);
UPDATE dml_row_assignment SET (a, b) = (1, 2, 3);
UPDATE dml_row_assignment SET a = 1, a = 2;
UPDATE dml_row_assignment SET (a, a) = (1, 2);
UPDATE dml_row_assignment SET (a, b) = (SELECT 1, 2 UNION ALL SELECT 3, 4);

DROP TABLE dml_row_assignment_source;
DROP TABLE dml_row_assignment;
