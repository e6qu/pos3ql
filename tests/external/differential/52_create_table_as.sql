-- CREATE TABLE ... AS <query> [WITH [NO] DATA]: build a table from a query's
-- output schema and populate it by running the query once, matching
-- PostgreSQL 18. The new table is an ordinary table (later INSERT/DDL work).
DROP TABLE IF EXISTS src;
CREATE TABLE src (id int, name text, amount numeric(8,2));
INSERT INTO src VALUES (1, 'alice', 10.50), (2, 'bob', 20.25), (3, 'carol', 5.00);

-- Basic CTAS with a WHERE and ORDER BY; columns and types come from the query.
DROP TABLE IF EXISTS t1;
CREATE TABLE t1 AS SELECT id, name FROM src WHERE amount > 6 ORDER BY id;
SELECT * FROM t1 ORDER BY id;

-- Expression columns: a computed numeric and a text function.
DROP TABLE IF EXISTS t2;
CREATE TABLE t2 AS SELECT id, amount * 2 AS doubled, upper(name) AS uname FROM src ORDER BY id;
SELECT * FROM t2 ORDER BY id;

-- A column-name list renames the query's output columns.
DROP TABLE IF EXISTS t3;
CREATE TABLE t3 (a, b) AS SELECT id, name FROM src ORDER BY id;
SELECT a, b FROM t3 ORDER BY a;

-- WITH NO DATA creates the table empty.
DROP TABLE IF EXISTS t4;
CREATE TABLE t4 AS SELECT id, name FROM src WITH NO DATA;
SELECT count(*) FROM t4;

-- IF NOT EXISTS: the second create is skipped, keeping the first table's data.
DROP TABLE IF EXISTS t5;
CREATE TABLE IF NOT EXISTS t5 AS SELECT 1 AS x;
CREATE TABLE IF NOT EXISTS t5 AS SELECT 2 AS x;
SELECT x FROM t5;

-- An aggregate query.
DROP TABLE IF EXISTS t6;
CREATE TABLE t6 AS SELECT count(*) AS n, sum(amount) AS total FROM src;
SELECT n, total FROM t6;

-- A set-operation body.
DROP TABLE IF EXISTS t7;
CREATE TABLE t7 AS SELECT id FROM src WHERE id = 1 UNION SELECT id FROM src WHERE id = 3;
SELECT id FROM t7 ORDER BY id;

-- A VALUES body: its columns are named column1, column2 as PostgreSQL names them.
DROP TABLE IF EXISTS t8;
CREATE TABLE t8 AS VALUES (10, 'x'), (20, 'y');
SELECT column1, column2 FROM t8 ORDER BY column1;

-- The created table is ordinary: INSERT works and column types are enforced.
INSERT INTO t1 VALUES (99, 'dan');
SELECT * FROM t1 ORDER BY id;
INSERT INTO t1 VALUES ('not an int', 'x');

DROP TABLE t1;
DROP TABLE t2;
DROP TABLE t3;
DROP TABLE t4;
DROP TABLE t5;
DROP TABLE t6;
DROP TABLE t7;
DROP TABLE t8;
DROP TABLE src;
