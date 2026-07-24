-- Composite field access and expansion: `(expr).field` and `(expr).*` bind
-- to the expression's static type, exactly as PostgreSQL — anonymous ROW()
-- records have no named fields (only star expansion works), whole-row
-- references resolve against their table, record SRFs expose their declared
-- columns, and non-composite values refuse column notation by type name.
CREATE TABLE rt(a int, b text);
INSERT INTO rt VALUES (1, 'x'), (2, 'y');

-- Whole-row field access, all spellings.
SELECT (rt.*).a FROM rt ORDER BY 1;
SELECT (rt).a FROM rt ORDER BY 1;
SELECT ((rt)).b FROM rt ORDER BY 1;
SELECT (rt.*).* FROM rt ORDER BY a;
SELECT (rt).* FROM rt ORDER BY a;
SELECT (rt.*).*, 5 FROM rt ORDER BY a;
SELECT pg_typeof((rt.*).a) FROM rt LIMIT 1;
SELECT * FROM rt WHERE (rt).a = 1;
SELECT (rt).a FROM rt ORDER BY (rt).a DESC;
SELECT count((rt).a) FROM rt;
SELECT (rt.*).a + (rt).a FROM rt ORDER BY 1;

-- Missing fields and non-composite bases.
SELECT (rt.*).nosuch FROM rt;
SELECT (rt.a).b FROM rt;
SELECT ('x'::text).y;
SELECT ((rt.*).a).nosuch FROM rt;

-- Anonymous ROW(): star expansion works, named fields never do.
SELECT (ROW(1,2)).*;
SELECT (ROW(1,'a'::text)).*;
SELECT (ROW(1,2)).f1;
SELECT (ROW(1,2)).nosuch;
SELECT (ROW(1,'a')).f1;
SELECT (ROW(1,'a')).*;
SELECT (ROW(1,ROW(2,3))).f2.f1;
SELECT (CASE WHEN true THEN ROW(1,2) END).f1;
SELECT ((SELECT ROW(4,5))).f1;
SELECT (min(rt.*)).a FROM rt;
SELECT (ROW(1,2)).* + 1;

-- Record SRFs expose their declared columns.
SELECT (json_each('{"k":1,"j":2}')).*;
SELECT (json_each('{"k":1}')).key;
SELECT (json_each('{"k":1}')).value;
SELECT pg_typeof((json_each('{"k":1}')).value);
SELECT (jsonb_each_text('{"k":1}')).*;
SELECT (json_array_elements('[1,2]')).*;

-- Derived tables and CTEs: whole-row access over their columns.
SELECT (v).a, (v.*).b FROM (SELECT 1 AS a, 'z'::text AS b) v;
SELECT (v).* FROM (SELECT 1 AS a, 'z'::text AS b) v;
WITH w AS (SELECT 7 AS q) SELECT (w).q FROM w;

-- RETURNING carries whole-row field access too.
INSERT INTO rt VALUES (3, 'z') RETURNING (rt.*).b;

DROP TABLE rt;

-- Record-typed derived-table columns: selection, field access, ordering,
-- distinctness, and nesting — the row type flows through the subquery.
SELECT q FROM (SELECT ROW(1,'t'::text) AS q) s;
SELECT (q).f1, (q).f2 FROM (SELECT ROW(1,'t'::text) AS q) s;
SELECT pg_typeof((q).f1), pg_typeof((q).f2) FROM (SELECT ROW(1,'t'::text) AS q) s;
SELECT (q).* FROM (SELECT ROW(1,'t'::text) AS q) s;
SELECT (q).f1 + 1 FROM (SELECT ROW(1,2) AS q) s;
SELECT (q).nosuch FROM (SELECT ROW(1,2) AS q) s;
SELECT (c).key, (c).value FROM (SELECT json_each('{"k":5,"j":6}') AS c) sub ORDER BY 1;
SELECT (c).* FROM (SELECT json_each('{"k":5}') AS c) sub;
CREATE TABLE rt2(a int, b text);
INSERT INTO rt2 VALUES (2,'y'),(1,'x');
SELECT (v).r, ((v).r).a FROM (SELECT rt2 AS r FROM rt2) v ORDER BY 2;
SELECT ((v).r).b FROM (SELECT rt2 AS r FROM rt2) v ORDER BY 1;
SELECT q FROM (SELECT ROW(2,'b'::text) AS q UNION ALL SELECT ROW(1,'a'::text)) s ORDER BY q;
SELECT DISTINCT q FROM (SELECT ROW(1,2) AS q UNION ALL SELECT ROW(1,2)) s;
SELECT count(q) FROM (SELECT ROW(1,2) AS q) s;
SELECT (q).f2.f1 FROM (SELECT ROW(1,ROW(2,3)) AS q) s;
WITH w AS (SELECT ROW(9,'z'::text) AS q) SELECT (q).f1 FROM w;
DROP TABLE rt2;

-- Three-part column references: schema.table.column binds only to an
-- unaliased FROM entry whose base table really is that schema's table.
CREATE TABLE rt3(a int, b text);
INSERT INTO rt3 VALUES (1,'x');
SELECT public.rt3.a FROM rt3;
SELECT public.rt3.a, rt3.b FROM public.rt3;
SELECT nosuch.rt3.a FROM rt3;
SELECT public.rt3.a FROM rt3 AS aliased;
SELECT public.aliased.a FROM rt3 AS aliased;
SELECT pg_catalog.rt3.a FROM rt3;
SELECT public.rt3.a FROM rt3 WHERE public.rt3.b = 'x';
SELECT public.rt3.a, count(*) FROM rt3 GROUP BY public.rt3.a;
CREATE SCHEMA s3p;
CREATE TABLE s3p.rt3(z int);
INSERT INTO s3p.rt3 VALUES (7);
SELECT s3p.rt3.z FROM s3p.rt3;
SELECT public.rt3.a FROM s3p.rt3;
DROP SCHEMA s3p CASCADE;
DROP TABLE rt3;
