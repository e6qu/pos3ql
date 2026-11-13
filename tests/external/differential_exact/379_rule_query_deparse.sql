-- pg_get_ruledef, in both modes, and pg_rules print PostgreSQL's deparsed
-- form of rule actions that read subqueries, functions (implicitly LATERAL
-- when they read NEW), explicit joins, set operations, DISTINCT ON,
-- windows, grouping sets, WITH queries, sublinks and VALUES lists. A set
-- operation member of an action cannot read OLD or NEW.
CREATE TABLE t379 (a int, b text);
CREATE TABLE l379 (a int, b text);
CREATE TABLE x379 (a int, c text);
CREATE RULE r379_1 AS ON INSERT TO t379 DO ALSO INSERT INTO l379 SELECT s.a, s.c FROM (SELECT a, c FROM x379) s WHERE s.a = new.a;
CREATE RULE r379_2 AS ON INSERT TO t379 DO ALSO INSERT INTO l379 SELECT g, 'x' FROM generate_series(1, new.a) g;
CREATE RULE r379_3 AS ON UPDATE TO t379 DO ALSO INSERT INTO l379 SELECT x379.a, x379.c FROM x379 JOIN l379 USING (a) WHERE x379.a = old.a;
CREATE RULE r379_4 AS ON DELETE TO t379 DO ALSO INSERT INTO l379 SELECT a, c FROM x379 UNION SELECT old.a, old.b;
CREATE RULE r379_5 AS ON INSERT TO t379 DO ALSO INSERT INTO l379 SELECT DISTINCT ON (a) a, c FROM x379 ORDER BY a, c;
CREATE RULE r379_6 AS ON INSERT TO t379 DO ALSO INSERT INTO l379 SELECT a, max(c) OVER (PARTITION BY a) FROM x379;
CREATE RULE r379_7 AS ON INSERT TO t379 DO ALSO INSERT INTO l379 SELECT a, max(c) FROM x379 GROUP BY ROLLUP (a);
CREATE RULE r379_8 AS ON UPDATE TO t379 DO ALSO UPDATE l379 SET b = s.c FROM (SELECT a, c FROM x379) s WHERE l379.a = s.a AND s.a = new.a;
CREATE RULE r379_9 AS ON DELETE TO t379 DO ALSO DELETE FROM l379 USING x379 JOIN t379 r2 ON x379.a = r2.a WHERE l379.a = x379.a AND l379.a = old.a;
CREATE RULE r379_10 AS ON INSERT TO t379 DO ALSO INSERT INTO l379 WITH w AS (SELECT a, c FROM x379) SELECT a, c FROM w;
CREATE RULE r379_11 AS ON INSERT TO t379 DO ALSO INSERT INTO l379 SELECT a, c FROM x379 WHERE EXISTS (SELECT 1 FROM l379 WHERE l379.a = new.a);
CREATE RULE r379_12 AS ON UPDATE TO t379 DO ALSO SELECT x.a FROM x379 x LEFT JOIN l379 ON l379.a = x.a WHERE x.a = new.a;
CREATE RULE r379_13 AS ON INSERT TO t379 DO ALSO INSERT INTO l379 SELECT v.x, 'y' FROM (VALUES (1), (2)) v(x);
CREATE RULE r379_14 AS ON INSERT TO t379 WHERE new.a > 0 DO INSTEAD INSERT INTO l379 VALUES (new.a, CASE WHEN new.b IS NULL THEN 'n' ELSE new.b END);
SELECT rulename, pg_get_ruledef(oid) FROM pg_rewrite WHERE ev_class = 't379'::regclass ORDER BY 1;
SELECT rulename, pg_get_ruledef(oid, true) FROM pg_rewrite WHERE ev_class = 't379'::regclass ORDER BY 1;
SELECT rulename, definition FROM pg_rules WHERE tablename = 't379' ORDER BY 1;
