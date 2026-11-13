-- EXPLAIN of a statement a rewrite rule changes plans every query the rule
-- produces: actions with their own FROM or USING items join the original
-- relation after them, INSERT ... SELECT actions read their own tables, the
-- original INSERT's several rows become a VALUES scan, and the structured
-- formats hold one query per plan, with a NOTIFY action as a Notify entry.
-- A conditional INSTEAD rule keeps the original for rows its condition does
-- not hold for.
CREATE TABLE t370 (a int, b text DEFAULT 'dflt');
CREATE TABLE s370 (a int, b text);
CREATE TABLE l370 (a int, b text);
CREATE TABLE x370 (a int, c text);
CREATE RULE i370 AS ON INSERT TO s370
  DO ALSO INSERT INTO l370 SELECT new.a, x370.c FROM x370 WHERE x370.a = new.a;
CREATE RULE r370 AS ON INSERT TO t370
  DO ALSO INSERT INTO l370 VALUES (new.a, new.b) RETURNING *;
CREATE RULE d370 AS ON DELETE TO t370
  DO ALSO DELETE FROM l370 WHERE l370.b = old.b;
CREATE RULE n370 AS ON DELETE TO t370 DO ALSO NOTIFY chan370;
EXPLAIN (COSTS OFF) INSERT INTO s370 VALUES (1, 'x');
EXPLAIN (COSTS OFF) INSERT INTO s370 SELECT 1, 'q';
EXPLAIN (COSTS OFF) INSERT INTO t370 VALUES (1, 'x'), (2, 'y');
EXPLAIN (COSTS OFF) INSERT INTO t370 VALUES (1, DEFAULT);
EXPLAIN (COSTS OFF) DELETE FROM t370 WHERE b = 'z';
EXPLAIN (COSTS OFF, FORMAT JSON) DELETE FROM t370 WHERE b = 'z';
EXPLAIN (COSTS OFF, FORMAT YAML) DELETE FROM t370 WHERE b = 'z';
EXPLAIN (COSTS OFF, FORMAT XML) INSERT INTO s370 VALUES (1, 'x');
CREATE TABLE c370 (a int, b text);
CREATE RULE c370_positive AS ON INSERT TO c370 WHERE new.a > 0
  DO INSTEAD INSERT INTO l370 VALUES (new.a, new.b);
CREATE RULE c370_update AS ON UPDATE TO c370 WHERE old.a < 0
  DO INSTEAD DELETE FROM l370 WHERE l370.a = old.a;
EXPLAIN (COSTS OFF) INSERT INTO c370 VALUES (1, 'x');
EXPLAIN (COSTS OFF) INSERT INTO c370 VALUES (-1, 'x');
EXPLAIN (COSTS OFF) INSERT INTO c370 SELECT a, b FROM l370;
EXPLAIN (COSTS OFF) UPDATE c370 SET b = 'y' WHERE b = 'x';
