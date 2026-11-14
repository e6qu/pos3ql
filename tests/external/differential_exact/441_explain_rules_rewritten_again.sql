-- EXPLAIN of a statement rewrite rules change prints every query
-- PostgreSQL's rewriter produces, with each rule action rewritten again by
-- the rules of its own target: an INSERT before its actions, an UPDATE or
-- DELETE after them. A query PostgreSQL cannot pull up into the INSERT
-- reading it is scanned as "*SELECT*", several VALUES rows as "*VALUES*",
-- and the views an action reads are read through their queries.
CREATE TABLE t441 (a int, b text);
CREATE TABLE l441 (a int, b text);
CREATE TABLE m441 (a int, b text);
CREATE TABLE x441 (a int, c text);
-- Without rules: the INSERT's source as PostgreSQL scans it.
EXPLAIN (COSTS OFF) INSERT INTO t441 SELECT count(*), 'x' FROM x441;
EXPLAIN (COSTS OFF) INSERT INTO t441 SELECT count(*)::int, 'x'::text FROM x441;
EXPLAIN (COSTS OFF) INSERT INTO t441 SELECT a, c FROM x441 ORDER BY a;
EXPLAIN (COSTS OFF) INSERT INTO t441 (b, a) SELECT c, a FROM x441 ORDER BY a;
EXPLAIN (COSTS OFF) INSERT INTO t441 (a) SELECT a FROM x441 ORDER BY a;
EXPLAIN (COSTS OFF) INSERT INTO t441 SELECT DISTINCT a, c FROM x441;
EXPLAIN (COSTS OFF) INSERT INTO t441 SELECT a, max(c) FROM x441 GROUP BY a;
EXPLAIN (COSTS OFF) INSERT INTO t441 SELECT s.a, s.c FROM (SELECT a, c FROM x441 ORDER BY a) s;
EXPLAIN (COSTS OFF) INSERT INTO t441 SELECT s.a, 'z' FROM (SELECT a FROM x441 ORDER BY a) s;
EXPLAIN (COSTS OFF) SELECT * FROM (VALUES (1, 'x'), (2, 'y')) v WHERE column2 = 'x' AND column1 > 1;
EXPLAIN (COSTS OFF) SELECT * FROM (VALUES (1), (2)) v (n) WHERE n > 0;
CREATE RULE t441_ins AS ON INSERT TO t441 DO ALSO INSERT INTO l441 VALUES (new.a, new.b);
CREATE RULE l441_ins AS ON INSERT TO l441 DO ALSO INSERT INTO m441 VALUES (new.a * 10, new.b);
EXPLAIN (COSTS OFF) INSERT INTO t441 VALUES (1, 'a');
EXPLAIN (COSTS OFF) INSERT INTO t441 VALUES (1, 'a'), (2, 'b');
EXPLAIN (COSTS OFF) INSERT INTO t441 SELECT a, c FROM x441;
EXPLAIN (COSTS OFF) INSERT INTO t441 SELECT count(*), 'x' FROM x441;
EXPLAIN (COSTS OFF) INSERT INTO t441 SELECT a, c FROM x441 ORDER BY a;
EXPLAIN (COSTS OFF, FORMAT JSON) INSERT INTO t441 VALUES (1, 'a');
-- A conditional INSTEAD rule keeps the rows its condition does not take.
CREATE TABLE c441 (a int, b text);
CREATE RULE c441_pos AS ON INSERT TO c441 WHERE new.a > 0
  DO INSTEAD INSERT INTO l441 VALUES (new.a, new.b);
EXPLAIN (COSTS OFF) INSERT INTO c441 VALUES (1, 'x'), (-1, 'y');
EXPLAIN (COSTS OFF) INSERT INTO c441 VALUES (1, 'x');
-- A statement on a view with rules, and an action on such a view.
CREATE VIEW v441 AS SELECT a, b FROM t441;
CREATE RULE v441_ins AS ON INSERT TO v441 DO INSTEAD INSERT INTO t441 VALUES (new.a, new.b);
EXPLAIN (COSTS OFF) INSERT INTO v441 VALUES (5, 'v');
CREATE TABLE s441 (a int, b text);
CREATE RULE s441_ins AS ON INSERT TO s441 DO ALSO INSERT INTO v441 VALUES (new.a + 100, new.b);
EXPLAIN (COSTS OFF) INSERT INTO s441 VALUES (7, 's');
-- An auto-updatable view's base table runs its rules.
CREATE VIEW l441v AS SELECT a, b FROM l441;
EXPLAIN (COSTS OFF) INSERT INTO l441v VALUES (1, 'v');
-- An action reading a view reads its query; a SELECT action is a query.
CREATE VIEW m441v AS SELECT a, b FROM m441 WHERE a > 0;
CREATE TABLE r441 (a int, b text);
CREATE TABLE rl441 (a int, n int);
CREATE RULE r441_ins AS ON INSERT TO r441 DO ALSO INSERT INTO rl441 SELECT new.a, m441v.a FROM m441v;
CREATE RULE r441_sel AS ON INSERT TO r441 DO ALSO SELECT new.a, new.b;
EXPLAIN (COSTS OFF) INSERT INTO r441 VALUES (5, 'r');
-- A DELETE action that reads nothing of the deleted relation runs alone.
CREATE TABLE d441 (a int, b text);
CREATE RULE d441_del AS ON DELETE TO d441 DO ALSO DELETE FROM m441;
EXPLAIN (COSTS OFF) DELETE FROM d441;
-- Recursion fails while rewriting.
CREATE TABLE ra441 (a int);
CREATE TABLE rb441 (a int);
CREATE RULE ra441_ins AS ON INSERT TO ra441 DO ALSO INSERT INTO rb441 VALUES (new.a);
CREATE RULE rb441_ins AS ON INSERT TO rb441 DO ALSO INSERT INTO ra441 VALUES (new.a);
EXPLAIN (COSTS OFF) INSERT INTO ra441 VALUES (1);
