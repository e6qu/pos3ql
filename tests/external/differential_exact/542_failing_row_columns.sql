-- A NOT NULL, CHECK or view check option violation's "Failing row contains"
-- detail lists, for a user without SELECT on the table, only the columns the
-- user may read or the statement wrote, named, and nothing while row
-- security applies to the user (PostgreSQL's ExecBuildSlotValueDescription).
-- The details are reported as notices, which the comparison reads.
CREATE ROLE c542_writer;
CREATE FUNCTION c542_try(statement text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE detail text; relation text;
BEGIN
  EXECUTE statement;
EXCEPTION WHEN OTHERS THEN
  GET STACKED DIAGNOSTICS detail = PG_EXCEPTION_DETAIL, relation = TABLE_NAME;
  RAISE NOTICE '% [%] %', SQLSTATE, relation, detail;
END $$;
GRANT EXECUTE ON FUNCTION c542_try(text) TO c542_writer;
CREATE TABLE c542 (a int, b int NOT NULL, c int CHECK (c > 0), d text);
INSERT INTO c542 VALUES (1, 1, 1, 'x');
GRANT INSERT, UPDATE ON c542 TO c542_writer;
GRANT SELECT (a) ON c542 TO c542_writer;
SET ROLE c542_writer;
SELECT c542_try('INSERT INTO c542 (a, c) VALUES (2, 1)');
SELECT c542_try('INSERT INTO c542 (b, c) VALUES (2, 0)');
SELECT c542_try('INSERT INTO c542 VALUES (2, NULL)');
SELECT c542_try('INSERT INTO c542 (d) VALUES (''y'')');
SELECT c542_try('UPDATE c542 SET c = -1');
SELECT c542_try('UPDATE c542 SET b = NULL WHERE a = 1');
SELECT c542_try('INSERT INTO c542 VALUES (1, 1, 1) ON CONFLICT DO NOTHING');
COPY c542 (b, c) FROM STDIN;
3	-3
\.
RESET ROLE;
GRANT SELECT ON c542 TO c542_writer;
SET ROLE c542_writer;
SELECT c542_try('INSERT INTO c542 (a, c) VALUES (2, 1)');
SELECT c542_try('UPDATE c542 SET c = -1');
RESET ROLE;
CREATE VIEW c542_v AS SELECT a, b, c FROM c542 WHERE a > 0 WITH CHECK OPTION;
GRANT INSERT, UPDATE ON c542_v TO c542_writer;
REVOKE SELECT ON c542 FROM c542_writer;
SET ROLE c542_writer;
SELECT c542_try('INSERT INTO c542_v (a, b, c) VALUES (-1, 1, 1)');
SELECT c542_try('UPDATE c542_v SET a = -5');
RESET ROLE;
ALTER TABLE c542 ENABLE ROW LEVEL SECURITY;
CREATE POLICY c542_all ON c542 USING (true) WITH CHECK (true);
GRANT SELECT ON c542 TO c542_writer;
SET ROLE c542_writer;
SELECT c542_try('INSERT INTO c542 (a, c) VALUES (2, 1)');
SELECT c542_try('UPDATE c542 SET c = -1');
SELECT c542_try('MERGE INTO c542 t USING (SELECT 1 AS k) s ON t.a = s.k WHEN MATCHED THEN UPDATE SET c = 0');
RESET ROLE;
ALTER TABLE c542 DISABLE ROW LEVEL SECURITY;
REVOKE SELECT ON c542 FROM c542_writer;
SET ROLE c542_writer;
SELECT c542_try('MERGE INTO c542 t USING (SELECT 1 AS k) s ON t.a = s.k WHEN MATCHED THEN UPDATE SET c = 0');
SELECT c542_try('MERGE INTO c542 t USING (SELECT 9 AS k) s ON t.a = s.k WHEN NOT MATCHED THEN INSERT (a, b) VALUES (9, NULL)');
RESET ROLE;
DROP VIEW c542_v;
DROP TABLE c542;
DROP FUNCTION c542_try(text);
DROP ROLE c542_writer;
