-- PostgreSQL's rewriter applies rules to every rule action again: an
-- action on a relation with rules runs that relation's actions too, in
-- RewriteQuery's order (an INSERT before its actions, an UPDATE or DELETE
-- after them), and each action sees the changes of the queries before it.
-- Recursion through rules fails before anything runs.
CREATE TABLE t440 (a int, b text);
CREATE TABLE l440 (a int, b text);
CREATE TABLE m440 (a int, b text);
CREATE FUNCTION note440() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE '% % % on %', TG_WHEN, TG_LEVEL, TG_OP, TG_TABLE_NAME;
  RETURN CASE WHEN TG_OP = 'DELETE' THEN OLD ELSE NEW END;
END $$;
CREATE TRIGGER t440_row BEFORE INSERT OR UPDATE OR DELETE ON t440
  FOR EACH ROW EXECUTE FUNCTION note440();
CREATE TRIGGER t440_statement AFTER INSERT OR UPDATE OR DELETE ON t440
  FOR EACH STATEMENT EXECUTE FUNCTION note440();
CREATE TRIGGER l440_row AFTER INSERT OR UPDATE OR DELETE ON l440
  FOR EACH ROW EXECUTE FUNCTION note440();
CREATE TRIGGER m440_row BEFORE INSERT OR UPDATE OR DELETE ON m440
  FOR EACH ROW EXECUTE FUNCTION note440();
-- Recursion is found while rewriting, before any trigger fires.
CREATE RULE t440_ins AS ON INSERT TO t440 DO ALSO INSERT INTO l440 VALUES (new.a, new.b);
CREATE RULE l440_back AS ON INSERT TO l440 DO ALSO INSERT INTO t440 VALUES (new.a, new.b);
INSERT INTO t440 VALUES (1, 'x');
EXPLAIN (COSTS OFF) INSERT INTO t440 VALUES (1, 'x');
CREATE RULE l440_self AS ON UPDATE TO l440 DO ALSO UPDATE l440 SET b = 'again';
UPDATE l440 SET b = 'y';
DROP RULE l440_back ON l440;
DROP RULE l440_self ON l440;
-- An action on a table with rules of its own runs them.
CREATE RULE l440_ins AS ON INSERT TO l440 DO ALSO INSERT INTO m440 VALUES (new.a * 10, new.b);
INSERT INTO t440 VALUES (1, 'x'), (2, 'y');
INSERT INTO t440 SELECT a + 2, b FROM t440 RETURNING *;
SELECT * FROM t440 ORDER BY a;
SELECT * FROM l440 ORDER BY a;
SELECT * FROM m440 ORDER BY a;
-- UPDATE and DELETE actions run before their statement, nested ones too.
CREATE RULE t440_upd AS ON UPDATE TO t440 DO ALSO UPDATE l440 SET b = new.b WHERE l440.a = old.a;
CREATE RULE l440_upd AS ON UPDATE TO l440 DO INSTEAD UPDATE m440 SET b = new.b || '!' WHERE m440.a = old.a * 10;
UPDATE t440 SET b = 'z' WHERE a = 1;
CREATE RULE t440_del AS ON DELETE TO t440 DO INSTEAD DELETE FROM l440 WHERE l440.a = old.a;
CREATE RULE l440_del AS ON DELETE TO l440 DO ALSO DELETE FROM m440 WHERE m440.a = old.a * 10;
DELETE FROM t440 WHERE a = 2;
SELECT * FROM t440 ORDER BY a;
SELECT * FROM l440 ORDER BY a;
SELECT * FROM m440 ORDER BY a;
-- The command tag counts the last INSTEAD action of the statement's own
-- command, here a DELETE two levels down.
CREATE TABLE q440 (a int);
CREATE RULE q440_del AS ON DELETE TO q440 DO INSTEAD DELETE FROM t440 WHERE t440.a = old.a;
INSERT INTO q440 VALUES (3), (4);
DELETE FROM q440;
SELECT count(*) FROM q440;
SELECT * FROM l440 ORDER BY a;
SELECT * FROM m440 ORDER BY a;
-- Each action sees the changes of the actions before it.
CREATE TABLE s440 (a int);
CREATE TABLE log440 (a int, n int);
CREATE RULE s440_a AS ON INSERT TO s440 DO ALSO (
  INSERT INTO log440 VALUES (new.a, 1);
  INSERT INTO log440 SELECT a, n + 10 FROM log440 WHERE a = new.a);
CREATE RULE s440_b AS ON INSERT TO s440 DO ALSO
  UPDATE log440 SET n = n + 100 WHERE a = new.a AND n > 10;
INSERT INTO s440 VALUES (5);
SELECT * FROM log440 ORDER BY n;
-- Actions read views; a SELECT action's * covers only its own FROM items.
CREATE VIEW m440v AS SELECT a, b FROM m440 WHERE a > 0;
CREATE TABLE r440 (a int, b text);
CREATE TABLE rl440 (a int, n bigint);
CREATE RULE r440_cnt AS ON INSERT TO r440 DO ALSO INSERT INTO rl440 SELECT new.a, count(*) FROM m440v;
CREATE RULE r440_cnt AS ON INSERT TO r440 DO ALSO INSERT INTO rl440 SELECT new.a, m440v.a FROM m440v;
CREATE RULE r440_del AS ON INSERT TO r440 DO ALSO DELETE FROM rl440 WHERE a IN (SELECT a FROM m440v WHERE b = 'q');
CREATE RULE r440_sel AS ON INSERT TO r440 DO ALSO SELECT * FROM m440v ORDER BY a;
INSERT INTO r440 VALUES (7, 'r');
SELECT * FROM rl440 ORDER BY n;
-- An auto-updatable view's base table runs its own rules.
CREATE VIEW l440v AS SELECT a, b FROM l440;
INSERT INTO l440v VALUES (8, 'v');
UPDATE l440v SET b = 'w' WHERE a = 8;
SELECT * FROM l440 ORDER BY a;
SELECT * FROM m440 ORDER BY a;
-- A view with only a conditional INSTEAD rule cannot be updated through
-- its query.
CREATE VIEW t440v AS SELECT a, b FROM t440;
CREATE RULE t440v_ins AS ON INSERT TO t440v WHERE new.a > 100 DO INSTEAD NOTHING;
INSERT INTO t440v VALUES (1, 'v');
-- A data-modifying WITH query must rewrite to exactly one statement,
-- counting the rules of the relations its actions write.
CREATE TABLE w440 (a int);
CREATE TABLE w440b (a int);
CREATE RULE w440_ins AS ON INSERT TO w440 DO INSTEAD INSERT INTO w440b VALUES (new.a);
WITH ins AS (INSERT INTO w440 VALUES (9) RETURNING *) SELECT 1;
WITH ins AS (INSERT INTO w440 VALUES (9)) SELECT 1;
CREATE RULE w440b_cond AS ON INSERT TO w440b WHERE new.a > 5 DO INSTEAD NOTHING;
WITH ins AS (INSERT INTO w440 VALUES (9)) SELECT 1;
CREATE RULE w440b_also AS ON INSERT TO w440b DO ALSO NOTIFY w440;
WITH ins AS (INSERT INTO w440 VALUES (9)) SELECT 1;
DROP RULE w440b_also ON w440b;
CREATE RULE w440b_none AS ON INSERT TO w440b DO INSTEAD NOTHING;
WITH ins AS (INSERT INTO w440 VALUES (9)) SELECT 1;
SELECT count(*) FROM w440b;
-- A rule action keeps its RETURNING list only when the statement it replaces
-- has one; the rules of the relation it writes then need RETURNING too.
CREATE TABLE r440_t (a int);
CREATE TABLE r440_u (a int);
CREATE VIEW r440_v AS SELECT a FROM r440_t;
CREATE RULE r440_vi AS ON INSERT TO r440_v DO INSTEAD INSERT INTO r440_t VALUES (new.a) RETURNING r440_t.*;
CREATE RULE r440_ti AS ON INSERT TO r440_t DO INSTEAD INSERT INTO r440_u VALUES (new.a);
INSERT INTO r440_v VALUES (1) RETURNING *;
INSERT INTO r440_v VALUES (2);
WITH w AS (INSERT INTO r440_v VALUES (3) RETURNING a) SELECT * FROM w;
WITH w AS (INSERT INTO r440_v VALUES (4)) SELECT 1;
SELECT * FROM r440_u ORDER BY a;
