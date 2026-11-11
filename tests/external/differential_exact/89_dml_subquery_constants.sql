-- A constant compared with a column in a subquery of INSERT, UPDATE or
-- DELETE converts to the column's type before any row is read: these tables
-- are empty, so only analysis can fail.
CREATE TABLE sc89(a int, b text); CREATE TABLE uc89(a int);
DELETE FROM sc89 WHERE a IN (SELECT a FROM uc89 WHERE a = 'q');
UPDATE sc89 SET b = 'x' WHERE a IN (SELECT a FROM uc89 WHERE a = 'q');
UPDATE sc89 SET a = (SELECT a FROM uc89 WHERE a = 'zz');
INSERT INTO sc89 VALUES ((SELECT a FROM uc89 WHERE a = 'r'), 'x');
UPDATE sc89 SET a = (SELECT a FROM uc89 WHERE a = 'zz') WHERE a IN (SELECT a FROM uc89 WHERE a = 'w');
DELETE FROM sc89 USING uc89 WHERE sc89.a = uc89.a AND EXISTS (SELECT 1 FROM uc89 x WHERE x.a = 'e');
INSERT INTO sc89 SELECT a, 'y' FROM uc89 WHERE a = 'k';
INSERT INTO sc89 VALUES (1, 'x') RETURNING (SELECT a FROM uc89 WHERE a = 'ret');
DELETE FROM sc89 WHERE a = 1 RETURNING (SELECT a FROM uc89 WHERE a = 'ret2');
UPDATE sc89 SET b = 'x' WHERE a = (SELECT a FROM uc89 WHERE a = '1');
