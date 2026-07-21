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
-- CREATE TABLE ... (LIKE source [INCLUDING ...])
CREATE TABLE lk_src(id int PRIMARY KEY, a text NOT NULL DEFAULT 'x', b int UNIQUE, c int CHECK (c > 0));
-- plain LIKE copies names, types and NOT NULL, and nothing else
CREATE TABLE lk_plain (LIKE lk_src);
INSERT INTO lk_plain(id, a, b, c) VALUES (1, 'p', 1, -5) RETURNING id, a, b, c;
INSERT INTO lk_plain(id, b, c) VALUES (2, 2, 2);
INSERT INTO lk_plain(id, a) VALUES (1, 'dup'), (1, 'dup2') RETURNING id;
-- INCLUDING DEFAULTS
CREATE TABLE lk_def (LIKE lk_src INCLUDING DEFAULTS);
INSERT INTO lk_def(id, b, c) VALUES (1, 1, 1) RETURNING a;
-- INCLUDING CONSTRAINTS copies CHECK, not the keys
CREATE TABLE lk_con (LIKE lk_src INCLUDING CONSTRAINTS);
INSERT INTO lk_con(id, a, c) VALUES (1, 'p', -5);
INSERT INTO lk_con(id, a, c) VALUES (1, 'p', 5) RETURNING c;
INSERT INTO lk_con(id, a, c) VALUES (1, 'q', 6) RETURNING id;
-- INCLUDING INDEXES copies PRIMARY KEY and UNIQUE
CREATE TABLE lk_idx (LIKE lk_src INCLUDING INDEXES);
INSERT INTO lk_idx(id, a, b) VALUES (1, 'p', 1) RETURNING id;
INSERT INTO lk_idx(id, a, b) VALUES (1, 'q', 2);
INSERT INTO lk_idx(id, a, b) VALUES (2, 'q', 1);
INSERT INTO lk_idx(id, a, c) VALUES (3, 'r', -1) RETURNING id;
-- INCLUDING ALL
CREATE TABLE lk_all (LIKE lk_src INCLUDING ALL);
INSERT INTO lk_all(id, b, c) VALUES (1, 1, 1) RETURNING a;
INSERT INTO lk_all(id, b, c) VALUES (1, 2, 2);
INSERT INTO lk_all(id, b, c) VALUES (2, 2, -3);
-- EXCLUDING undoes an earlier INCLUDING; options may repeat
CREATE TABLE lk_exc (LIKE lk_src INCLUDING ALL EXCLUDING DEFAULTS);
INSERT INTO lk_exc(id, b, c) VALUES (1, 1, 1);
-- LIKE interleaves with ordinary columns, keeping written order
CREATE TABLE lk_mix (z int, LIKE lk_src, w text);
INSERT INTO lk_mix VALUES (1, 2, 'a', 3, 4, 'w') RETURNING z, id, a, b, c, w;
-- secondary indexes come with INCLUDING INDEXES
CREATE TABLE lk_isrc(a int, b int);
CREATE UNIQUE INDEX lk_isrc_uq ON lk_isrc(a);
CREATE TABLE lk_icopy (LIKE lk_isrc INCLUDING INDEXES);
INSERT INTO lk_icopy(a, b) VALUES (1, 1), (1, 2);
INSERT INTO lk_icopy(a, b) VALUES (1, 1), (2, 2) RETURNING a;
CREATE TABLE lk_inone (LIKE lk_isrc);
INSERT INTO lk_inone(a, b) VALUES (1, 1), (1, 2) RETURNING a;
-- a table gets one primary key, however the two are written
CREATE TABLE lk_2pk(a int PRIMARY KEY, b int PRIMARY KEY);
CREATE TABLE lk_2pk(a int PRIMARY KEY, b int, PRIMARY KEY (b));
CREATE TABLE lk_2pk (LIKE lk_src INCLUDING INDEXES, z int PRIMARY KEY);
CREATE TABLE lk_1pk(a int PRIMARY KEY, b int UNIQUE);
DROP TABLE lk_1pk;
-- errors
CREATE TABLE lk_bad (LIKE nosuchtable);
CREATE TABLE lk_bad (LIKE lk_src INCLUDING BOGUS);
CREATE TABLE lk_bad (a int, LIKE lk_src);
DROP TABLE lk_icopy; DROP TABLE lk_inone; DROP TABLE lk_isrc;
DROP TABLE lk_mix; DROP TABLE lk_exc; DROP TABLE lk_all; DROP TABLE lk_idx;
DROP TABLE lk_con; DROP TABLE lk_def; DROP TABLE lk_plain; DROP TABLE lk_src;
