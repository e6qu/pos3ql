-- `t.*` continues into an expression over the whole row, `t.* AS x` still
-- expands, and DROP lists accept database-qualified names.
CREATE TABLE w181(a int, b text);
INSERT INTO w181 VALUES (1, 'x'), (NULL, NULL);
SELECT w181.* IS NULL, w181.* IS NOT NULL, w181 IS NULL FROM w181 ORDER BY 1;
SELECT w181.* AS q FROM w181 ORDER BY 1;
SELECT w181.*, 1 FROM w181 ORDER BY 1;
CREATE SCHEMA s181; CREATE TABLE s181.t1(a int); CREATE TABLE s181.t2(a int);
DROP TABLE postgres.s181.t1, nodb.s181.t2;
DROP TABLE postgres.s181.t1;
DROP TABLE a.b.c.d;
DROP VIEW postgres.s181.nov;
DROP TYPE a.b.c.d;
DROP TABLE IF EXISTS postgres.s181.t2;
DROP SCHEMA s181;
DROP TABLE w181;
