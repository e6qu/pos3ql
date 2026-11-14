-- A key violation's DETAIL shows the key's values only to a user who may
-- read them: with SELECT on the table or on every key column, and never
-- while row security applies to the user (PostgreSQL's
-- BuildIndexValueDescription, ExecBuildSlotPartitionKeyDescription and
-- ri_ReportViolation). Otherwise a unique violation has no detail and the
-- other violations say less. The details are reported as notices, which
-- the comparison reads.
CREATE ROLE c512_writer;
CREATE FUNCTION c512_try(statement text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE detail text; relation text;
BEGIN
  EXECUTE statement;
EXCEPTION WHEN OTHERS THEN
  GET STACKED DIAGNOSTICS detail = PG_EXCEPTION_DETAIL, relation = TABLE_NAME;
  RAISE NOTICE '% [%] %', SQLSTATE, relation, detail;
END $$;
GRANT EXECUTE ON FUNCTION c512_try(text) TO c512_writer;
CREATE TABLE c512_u (id int PRIMARY KEY, a int, b int UNIQUE);
CREATE UNIQUE INDEX c512_u_expr ON c512_u ((a + 1));
INSERT INTO c512_u VALUES (1, 1, 1);
GRANT INSERT, UPDATE, DELETE ON c512_u TO c512_writer;
GRANT SELECT (id) ON c512_u TO c512_writer;
SET ROLE c512_writer;
SELECT c512_try('INSERT INTO c512_u VALUES (1, 2, 2)');
SELECT c512_try('INSERT INTO c512_u VALUES (2, 3, 1)');
SELECT c512_try('INSERT INTO c512_u VALUES (3, 1, 3)');
RESET ROLE;
CREATE TABLE c512_p (id int, v text) PARTITION BY RANGE (id);
CREATE TABLE c512_p1 PARTITION OF c512_p FOR VALUES FROM (0) TO (10);
GRANT INSERT ON c512_p TO c512_writer;
SET ROLE c512_writer;
SELECT c512_try($$INSERT INTO c512_p VALUES (20, 'x')$$);
RESET ROLE;
GRANT SELECT (id) ON c512_p TO c512_writer;
SET ROLE c512_writer;
SELECT c512_try($$INSERT INTO c512_p VALUES (20, 'x')$$);
RESET ROLE;
CREATE TABLE c512_parent (id int PRIMARY KEY);
CREATE TABLE c512_child (id int, pid int REFERENCES c512_parent);
INSERT INTO c512_parent VALUES (1);
INSERT INTO c512_child VALUES (1, 1);
GRANT INSERT, DELETE ON c512_child, c512_parent TO c512_writer;
SET ROLE c512_writer;
SELECT c512_try('INSERT INTO c512_child VALUES (2, 5)');
SELECT c512_try('DELETE FROM c512_parent');
RESET ROLE;
GRANT SELECT ON c512_child, c512_parent TO c512_writer;
SET ROLE c512_writer;
SELECT c512_try('INSERT INTO c512_child VALUES (2, 5)');
SELECT c512_try('DELETE FROM c512_parent');
RESET ROLE;
CREATE TABLE c512_x (r int4range, EXCLUDE USING gist (r WITH &&));
INSERT INTO c512_x VALUES ('[1,5)');
GRANT INSERT ON c512_x TO c512_writer;
SET ROLE c512_writer;
SELECT c512_try($$INSERT INTO c512_x VALUES ('[2,3)')$$);
RESET ROLE;
-- Row security withholds the key even from a reader of the whole table.
ALTER TABLE c512_u ENABLE ROW LEVEL SECURITY;
CREATE POLICY c512_all ON c512_u USING (true) WITH CHECK (true);
GRANT SELECT ON c512_u TO c512_writer;
SET ROLE c512_writer;
SELECT c512_try('INSERT INTO c512_u VALUES (1, 5, 5)');
RESET ROLE;
SELECT c512_try('INSERT INTO c512_u VALUES (1, 5, 5)');
DROP TABLE c512_u, c512_p, c512_child, c512_parent, c512_x;
DROP FUNCTION c512_try(text);
DROP ROLE c512_writer;
