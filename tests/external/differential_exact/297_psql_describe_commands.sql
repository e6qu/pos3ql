-- psql describe commands over catalog rows PostgreSQL ships or initdb creates:
-- the plpgsql extension, shipped object descriptions, access method members,
-- policies and column comments.
CREATE ROLE describe_reader;
CREATE TABLE describe_parent (id int PRIMARY KEY, label text DEFAULT 'x' NOT NULL);
COMMENT ON COLUMN describe_parent.label IS 'shown by \d+';
CREATE TABLE describe_child (parent_id int REFERENCES describe_parent (id), qty int UNIQUE);
GRANT SELECT ON describe_parent TO describe_reader;
GRANT UPDATE (label) ON describe_parent TO describe_reader;
ALTER TABLE describe_parent ENABLE ROW LEVEL SECURITY;
CREATE POLICY describe_policy ON describe_parent FOR SELECT TO describe_reader USING (id > 0);
CREATE SEQUENCE describe_sequence;
CREATE AGGREGATE describe_sum(int) (sfunc = int4pl, stype = int);
CREATE FOREIGN DATA WRAPPER describe_wrapper;
CREATE SERVER describe_server FOREIGN DATA WRAPPER describe_wrapper;
CREATE FOREIGN TABLE describe_foreign (a int) SERVER describe_server;
\d describe_parent
\d+ describe_parent
\d describe_child
\d+ describe_foreign
\dp
\z describe_parent
\dx
\dx+
\dL
\dL+
\dn+ public
\dFp
\df+ describe_sum
\dAc btree
\dAf btree
\dAo btree integer_ops
\dAp btree float_ops
\dA+
SELECT name, default_version, comment FROM pg_available_extensions() ORDER BY name;
SELECT (e).name, (e).version, (e).requires
  FROM (SELECT pg_available_extension_versions() AS e) AS versions ORDER BY 1, 2;
SELECT lanname, lanispl, lanpltrusted, lanplcallfoid::regproc, laninline::regproc,
       lanvalidator::regproc
  FROM pg_language ORDER BY lanname;
SELECT extname, extowner::regrole, extnamespace::regnamespace, extrelocatable, extversion,
       extconfig, extcondition
  FROM pg_extension;
SELECT pg_describe_object(classid, objid, objsubid) AS dependent,
       pg_describe_object(refclassid, refobjid, refobjsubid) AS referenced, deptype
  FROM pg_depend
 WHERE refclassid = 'pg_extension'::regclass OR classid = 'pg_language'::regclass
 ORDER BY 1, 2;
SELECT obj_description('public'::regnamespace, 'pg_namespace'),
       obj_description('int4'::regtype, 'pg_type'),
       obj_description((SELECT oid FROM pg_am WHERE amname = 'btree'), 'pg_am'),
       obj_description((SELECT oid FROM pg_extension WHERE extname = 'plpgsql'), 'pg_extension');
COMMENT ON SCHEMA public IS 'replaced';
SELECT obj_description('public'::regnamespace, 'pg_namespace');
COMMENT ON SCHEMA public IS NULL;
SELECT obj_description('public'::regnamespace, 'pg_namespace'),
       count(*) FROM pg_description WHERE objoid = 'public'::regnamespace;
CREATE EXTENSION plpgsql;
CREATE EXTENSION IF NOT EXISTS plpgsql;
SELECT pg_table_size('describe_sequence') IS NOT NULL,
       pg_relation_size('describe_parent') IS NOT NULL,
       pg_relation_size('describe_parent', 'fsm') IS NOT NULL,
       pg_total_relation_size('describe_parent') IS NOT NULL,
       pg_indexes_size('describe_parent') IS NOT NULL,
       pg_relation_size(0), pg_indexes_size(0);
SELECT pg_relation_size('describe_parent', 'bogus');
SELECT f.opfname,
       (SELECT string_agg(format_type(c.opcintype, NULL), ', ') FROM pg_opclass c
         WHERE c.opcfamily = f.oid) AS types
  FROM pg_opfamily f WHERE f.opfname IN ('float_ops', 'integer_ops', 'text_ops')
 ORDER BY f.opfmethod, f.opfname;
-- Boolean reloptions accept parse_bool's spellings and reject others.
CREATE VIEW describe_bad_option WITH (security_barrier = 'maybe') AS SELECT 1 AS a;
CREATE VIEW describe_bad_option WITH (security_barrier = ' true') AS SELECT 1 AS a;
-- A subquery nested in another subquery reads the enclosing subquery's row.
SELECT (SELECT (SELECT p.x) FROM (VALUES (1)) AS p(x));
SELECT (SELECT array(SELECT p.x + o.y) FROM (VALUES (1), (3)) AS p(x) ORDER BY 1 DESC LIMIT 1)
  FROM (VALUES (2)) AS o(y);
SELECT EXISTS (SELECT 1 FROM (VALUES (1)) AS p(x) WHERE (SELECT p.x) = 1);
SELECT (SELECT (SELECT o.y) FROM (VALUES (1)) AS p(x) GROUP BY p.x) FROM (VALUES (2)) AS o(y);
SELECT (SELECT array_agg(p.x) FROM (VALUES (1), (5)) AS p(x) WHERE (SELECT o.y + p.x) > 4)
  FROM (VALUES (2)) AS o(y);
DROP FOREIGN TABLE describe_foreign;
DROP SERVER describe_server;
DROP FOREIGN DATA WRAPPER describe_wrapper;
DROP AGGREGATE describe_sum(int);
DROP SEQUENCE describe_sequence;
DROP TABLE describe_child, describe_parent;
DROP ROLE describe_reader;
