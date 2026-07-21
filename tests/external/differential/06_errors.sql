-- Errors must carry the same SQLSTATEs.
SELECT 1/0;
SELECT 1.0/0;
SELECT * FROM missing_table;
CREATE TABLE e6 (id int NOT NULL, v text);
SELECT no_col FROM e6;
INSERT INTO e6 VALUES (NULL, 'x');
INSERT INTO e6 (nope) VALUES (1);
CREATE TABLE e6 (id int);
SELECT 'zap'::int;
SELECT 1::nosuchtype;
SELECT 9223372036854775807 + 1;
SELECT nosuchfunc(1);
SELECT count(*), id FROM e6;
SELECT 'x' FROM e6 e JOIN e6 f ON true;
DROP TABLE e6;
-- keyword categories: what may name a column, table, or alias (PostgreSQL's
-- ColId), versus what only needs quoting on output
-- reserved (R) and reserved-can-be-function-or-type (T) are rejected as names
CREATE TABLE kw_r(select int);
CREATE TABLE kw_r2(from int);
CREATE TABLE kw_r3(window int);
CREATE TABLE kw_r4(all int);
CREATE TABLE kw_r5(array int);
CREATE TABLE kw_r6(null int);
CREATE TABLE kw_t(left int);
CREATE TABLE kw_t2(natural int);
CREATE TABLE kw_t3(authorization int);
CREATE TABLE select(a int);
-- the two unreserved categories are legal names, and quoting always is
CREATE TABLE kw_ok(insert int, update int, between int, values int, name int, abort int);
INSERT INTO kw_ok VALUES (1,2,3,4,5,6);
SELECT insert, update, between, values, name, abort FROM kw_ok;
SELECT kw_ok.insert, kw_ok.between FROM kw_ok;
CREATE TABLE kw_q("select" int, "from" int);
INSERT INTO kw_q VALUES (10, 20);
SELECT "select", "from" FROM kw_q;
DROP TABLE kw_q;
-- a FROM-item alias is a ColId; a select-list alias is not
SELECT * FROM kw_ok AS insert;
SELECT * FROM kw_ok insert;
SELECT * FROM kw_ok AS select;
SELECT * FROM (SELECT 1) AS select;
SELECT * FROM (SELECT 1 a) AS insert;
SELECT 1 AS select;
SELECT 1 AS window;
WITH select AS (SELECT 1) SELECT * FROM select;
WITH insert AS (SELECT 1 a) SELECT a FROM insert;
-- a bare identifier in expression position follows the same rule, except that
-- a can-be-function-or-type keyword may still name a function
SELECT select FROM kw_ok;
SELECT left('abcdef', 3), right('abcdef', 3);
SELECT array[1,2,3], array(SELECT 1);
-- quote_ident quotes every keyword category except plain unreserved
SELECT quote_ident('abort'), quote_ident('insert'), quote_ident('between');
SELECT quote_ident('all'), quote_ident('left'), quote_ident('null'), quote_ident('array');
SELECT quote_ident('name'), quote_ident('not_a_keyword'), quote_ident('Mixed');
DROP TABLE kw_ok;
