-- pg_get_viewdef and pg_get_ruledef print queries that read functions
-- returning composite rows or column definition lists, catalog and
-- information schema relations, WITH queries with SEARCH and CYCLE, and
-- constant or repeated GROUP BY and ORDER BY keys; a rule on a view prints
-- its condition and actions over the view's columns. A non-integer constant
-- as a GROUP BY or ORDER BY key is rejected.
CREATE TABLE t421 (a int, b text, c numeric);
CREATE TYPE comp421 AS (x int, y text);
CREATE FUNCTION fc421() RETURNS comp421 LANGUAGE sql AS $$ SELECT 1, 'a'::text $$;
CREATE FUNCTION fsc421() RETURNS SETOF comp421 LANGUAGE sql AS $$ SELECT 1, 'a'::text $$;
CREATE FUNCTION ft421() RETURNS SETOF t421 LANGUAGE sql AS $$ SELECT * FROM t421 $$;
CREATE FUNCTION fo421(OUT p int, OUT q text) LANGUAGE sql AS $$ SELECT 1, 'a'::text $$;
CREATE VIEW v421_comp AS SELECT * FROM fc421();
CREATE VIEW v421_setof AS SELECT x, y FROM fsc421() f;
CREATE VIEW v421_rowtype AS SELECT f.a FROM ft421() f WHERE f.b = 'x';
CREATE VIEW v421_out AS SELECT * FROM fo421();
CREATE VIEW v421_json AS SELECT * FROM json_to_record('{"a":1}') AS r(a int, b text);
CREATE VIEW v421_jsonb AS SELECT r.a FROM jsonb_to_recordset('[{"a":1}]') AS r(a int, n numeric(5,2));
CREATE VIEW v421_catalog AS SELECT relname, relkind FROM pg_class c WHERE c.relnamespace = 11 ORDER BY 1;
CREATE VIEW v421_catalog2 AS SELECT c.relname, n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace;
CREATE VIEW v421_catalog3 AS SELECT * FROM pg_namespace;
CREATE VIEW v421_catalog4 AS SELECT table_name FROM information_schema.tables;
CREATE VIEW v421_search AS WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SEARCH DEPTH FIRST BY n SET ord SELECT n FROM r;
CREATE VIEW v421_cycle AS WITH RECURSIVE r(n, m) AS (SELECT 1, 2 UNION ALL SELECT n + 1, m FROM r WHERE n < 5) SEARCH BREADTH FIRST BY n, m SET ord CYCLE n SET is_cycle USING path SELECT n, is_cycle FROM r;
CREATE VIEW v421_cycle2 AS WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) CYCLE n SET is_cycle TO 'Y' DEFAULT 'N' USING path SELECT n, is_cycle FROM r;
CREATE VIEW v421_const AS SELECT a, 1 AS one FROM t421 GROUP BY a, 1 ORDER BY 2, 1;
CREATE VIEW v421_const2 AS SELECT a, 'x'::text AS k FROM t421 GROUP BY a, 2 ORDER BY 2;
CREATE VIEW v421_repeat AS SELECT a, count(*) FROM t421 GROUP BY 1, a ORDER BY 2, 2, 1;
CREATE VIEW v421_union AS SELECT a, 1 FROM t421 UNION SELECT c::int, a FROM t421;
CREATE VIEW v421_base AS SELECT a, b FROM t421;
CREATE RULE r421_ins AS ON INSERT TO v421_base DO INSTEAD INSERT INTO t421 (a, b) VALUES (new.a, new.b);
CREATE RULE r421_upd AS ON UPDATE TO v421_base WHERE old.a > 0 DO INSTEAD UPDATE t421 SET b = new.b WHERE t421.a = old.a;
SELECT a FROM t421 GROUP BY 'x';
SELECT a FROM t421 ORDER BY 1.5;
SELECT a FROM t421 ORDER BY NULL;
SELECT a FROM t421 ORDER BY true;
SELECT c.relname, pg_get_viewdef(c.oid) FROM pg_class c
  WHERE c.relkind = 'v' AND c.relname LIKE 'v421%' ORDER BY 1;
SELECT c.relname, pg_get_viewdef(c.oid, true) FROM pg_class c
  WHERE c.relkind = 'v' AND c.relname LIKE 'v421%' ORDER BY 1;
SELECT rulename, pg_get_ruledef(oid), pg_get_ruledef(oid, true) FROM pg_rewrite
  WHERE rulename LIKE 'r421%' ORDER BY 1;
