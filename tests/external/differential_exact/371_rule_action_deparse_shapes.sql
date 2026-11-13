-- pg_get_ruledef, in both modes, and pg_rules print PostgreSQL's deparsed
-- form of rule actions that read several tables, alias relations, use
-- ON CONFLICT, or group, filter groups, order, offset and limit rows.
CREATE TABLE t371 (a int, b text);
CREATE TABLE l371 (a int PRIMARY KEY, b text);
CREATE TABLE x371 (a int, c text);
CREATE TABLE y371 (a int, d text);
CREATE RULE r1 AS ON INSERT TO t371 DO ALSO INSERT INTO l371
  SELECT x371.a, y371.d FROM x371, y371 WHERE x371.a = y371.a AND x371.a = new.a;
CREATE RULE r2 AS ON UPDATE TO t371 DO ALSO UPDATE l371 AS q SET b = x371.c
  FROM x371, y371 z WHERE q.a = x371.a AND z.a = old.a;
CREATE RULE r3 AS ON DELETE TO t371 DO ALSO DELETE FROM l371 q USING x371 xx
  WHERE q.a = xx.a AND q.a = old.a;
CREATE RULE r4 AS ON INSERT TO t371 DO ALSO INSERT INTO l371 AS q VALUES (new.a, new.b)
  ON CONFLICT (a) DO UPDATE SET b = excluded.b WHERE q.b <> 'z';
CREATE RULE r5 AS ON INSERT TO t371 DO ALSO INSERT INTO l371
  SELECT a, max(c) FROM x371 WHERE a > new.a GROUP BY a HAVING count(*) > 1
  ORDER BY a DESC LIMIT 3 OFFSET 1;
CREATE RULE r6 AS ON INSERT TO t371 DO ALSO INSERT INTO l371 VALUES (new.a, new.b)
  ON CONFLICT DO NOTHING;
CREATE RULE r7 AS ON UPDATE TO t371 DO ALSO INSERT INTO l371
  SELECT DISTINCT a, c FROM x371 xx ORDER BY 1;
SELECT rulename, pg_get_ruledef(oid) FROM pg_rewrite WHERE ev_class = 't371'::regclass ORDER BY 1;
SELECT rulename, pg_get_ruledef(oid, true) FROM pg_rewrite WHERE ev_class = 't371'::regclass ORDER BY 1;
SELECT rulename, definition FROM pg_rules WHERE tablename = 't371' ORDER BY 1;
