-- Casts, operator classes and operator families, built-in or user-defined,
-- are described and identified as PostgreSQL does.
SELECT oid, pg_describe_object('pg_cast'::regclass, oid, 0), pg_identify_object('pg_cast'::regclass, oid, 0)
FROM pg_cast WHERE oid < 16384 ORDER BY oid;
SELECT oid, pg_describe_object('pg_opclass'::regclass, oid, 0), pg_identify_object('pg_opclass'::regclass, oid, 0)
FROM pg_opclass WHERE oid < 16384 ORDER BY oid;
SELECT oid, pg_describe_object('pg_opfamily'::regclass, oid, 0), pg_identify_object('pg_opfamily'::regclass, oid, 0)
FROM pg_opfamily WHERE oid < 16384 ORDER BY oid;
CREATE TYPE cd_mood AS ENUM ('a', 'b');
CREATE FUNCTION cd_mood_text(cd_mood) RETURNS text LANGUAGE sql AS 'SELECT $1::text';
CREATE CAST (cd_mood AS text) WITH FUNCTION cd_mood_text(cd_mood);
SELECT pg_describe_object(c.tableoid, c.oid, 0), pg_identify_object(c.tableoid, c.oid, 0)
FROM pg_cast c WHERE castsource = 'cd_mood'::regtype;
SELECT refclassid::regclass, pg_describe_object(refclassid, refobjid, refobjsubid), deptype
FROM pg_depend WHERE classid = 'pg_cast'::regclass
  AND objid = (SELECT oid FROM pg_cast WHERE castsource = 'cd_mood'::regtype) ORDER BY 2;
CREATE SCHEMA cd_hidden;
CREATE OPERATOR FAMILY cd_family USING btree;
CREATE OPERATOR FAMILY cd_hidden.cd_family USING btree;
SELECT pg_describe_object('pg_opfamily'::regclass, f.oid, 0), pg_identify_object('pg_opfamily'::regclass, f.oid, 0)
FROM pg_opfamily f WHERE opfname = 'cd_family' ORDER BY 1;
DROP OPERATOR FAMILY cd_family USING btree;
DROP SCHEMA cd_hidden CASCADE;
DROP CAST (cd_mood AS text);
DROP FUNCTION cd_mood_text(cd_mood);
DROP TYPE cd_mood;
