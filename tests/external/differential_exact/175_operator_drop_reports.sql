-- DROP OPERATOR reports dependents through the pg_depend walk, with skip
-- notices first, and CREATE OPERATOR names a duplicate operator.
CREATE FUNCTION eqf(int, int) RETURNS bool LANGUAGE sql AS 'SELECT $1 = $2';
CREATE OPERATOR === (LEFTARG = int, RIGHTARG = int, FUNCTION = eqf);
CREATE VIEW ov AS SELECT 1 === 1 AS r;
DROP OPERATOR === (int, int);
DROP OPERATOR IF EXISTS ==== (int, int), === (int, int);
SELECT count(*) FROM pg_operator WHERE oprname = '===';
CREATE OPERATOR === (LEFTARG = int, RIGHTARG = int, FUNCTION = eqf);
CREATE OPERATOR !== (LEFTARG = int, RIGHTARG = int, FUNCTION = eqf, NEGATOR = ===);
DROP OPERATOR === (int, int);
DROP OPERATOR === (int, int) CASCADE;
SELECT oprname, oprnegate FROM pg_operator WHERE oprname IN ('===', '!==');
DROP OPERATOR !== (int, int);
DROP FUNCTION eqf(int, int);
