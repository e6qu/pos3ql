-- A statement the grammar cannot finish reports PostgreSQL's generic
-- `syntax error at or near "token"` or `syntax error at end of input`, and
-- scanner failures echo the scanned text. Definition errors raised after
-- parsing keep their own SQLSTATE and wording.
SELECT 1 +;
SELEC 1;
CREATE TABLE (a int);
SELECT * FROM;
INSERT INTO x VALUES 1;
SELECT x NOT 1;
SELECT CASE END;
ALTER TABLE t FOO;
SELECT 1 2;
BEGIN ISOLATION LEVEL FOO;
CREATE TABLE t (a int, );
SELECT f(a := );
SELECT interval '1' day to year;
SELECT interval '1' month to day;
UPDATE t SET;
VALUES (1),;
SELECT 1 `;
SELECT 1a;
SELECT 1ex;
SELECT 0x1G;
SELECT $1abc;
SELECT "";
CREATE FUNCTION syntax_f() RETURNS int LANGUAGE sql STABLE VOLATILE AS 'SELECT 1';
CREATE FUNCTION syntax_f() RETURNS int LANGUAGE sql LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION syntax_f() LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION syntax_f() RETURNS int LANGUAGE sql;
CREATE FUNCTION syntax_f() RETURNS int AS 'SELECT 1';
