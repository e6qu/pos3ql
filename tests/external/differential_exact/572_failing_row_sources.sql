-- A violation's "Failing row contains" detail follows PostgreSQL's
-- ExecBuildSlotValueDescription for its result relation: an INSERT without
-- a column list writes the leading columns its source fills, a query's `*`
-- counting each column it expands to, and a view written through its
-- INSTEAD OF trigger is described by the view's privileges and the columns
-- the statement wrote. The details are reported as notices.
CREATE ROLE c572_writer;
CREATE FUNCTION c572_try(statement text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE detail text; relation text;
BEGIN
  EXECUTE statement;
EXCEPTION WHEN OTHERS THEN
  GET STACKED DIAGNOSTICS detail = PG_EXCEPTION_DETAIL, relation = TABLE_NAME;
  RAISE NOTICE '% [%] %', SQLSTATE, relation, detail;
END $$;
GRANT EXECUTE ON FUNCTION c572_try(text) TO c572_writer;
CREATE TABLE c572 (a int, b int NOT NULL, c int CHECK (c > 0), d text);
CREATE TABLE c572_s (a int, b int);
INSERT INTO c572_s VALUES (5, NULL);
GRANT INSERT, UPDATE ON c572 TO c572_writer;
GRANT SELECT ON c572_s TO c572_writer;
GRANT SELECT (a) ON c572 TO c572_writer;
SET ROLE c572_writer;
SELECT c572_try('INSERT INTO c572 SELECT * FROM c572_s');
SELECT c572_try('INSERT INTO c572 SELECT a, b FROM c572_s');
SELECT c572_try('INSERT INTO c572 SELECT a FROM c572_s');
SELECT c572_try('INSERT INTO c572 SELECT 1, 1, -1');
SELECT c572_try('INSERT INTO c572 VALUES (1, 1, -1)');
SELECT c572_try('INSERT INTO c572 VALUES (1, NULL)');
SELECT c572_try('INSERT INTO c572 DEFAULT VALUES');
RESET ROLE;
CREATE VIEW c572_tv AS SELECT a, b, c FROM c572;
CREATE FUNCTION c572_tf() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RETURN NEW; END $$;
CREATE TRIGGER c572_tt INSTEAD OF INSERT OR UPDATE ON c572_tv FOR EACH ROW EXECUTE FUNCTION c572_tf();
CREATE VIEW c572_cv AS SELECT a, b, c FROM c572_tv WHERE a > 0 WITH CHECK OPTION;
GRANT INSERT, UPDATE ON c572_cv TO c572_writer;
GRANT SELECT (b) ON c572_cv TO c572_writer;
SET ROLE c572_writer;
SELECT c572_try('INSERT INTO c572_cv (a, c) VALUES (-1, 1)');
SELECT c572_try('INSERT INTO c572_cv VALUES (-1, 2, 3)');
RESET ROLE;
GRANT SELECT ON c572_cv TO c572_writer;
SET ROLE c572_writer;
SELECT c572_try('INSERT INTO c572_cv (a, c) VALUES (-1, 1)');
RESET ROLE;
DROP VIEW c572_cv;
DROP VIEW c572_tv;
DROP FUNCTION c572_tf();
DROP TABLE c572, c572_s;
DROP FUNCTION c572_try(text);
DROP ROLE c572_writer;
