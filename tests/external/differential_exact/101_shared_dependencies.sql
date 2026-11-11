-- pg_shdepend records owners other than the bootstrap superuser and the
-- roles named in ACLs; pg_describe_object names visible objects without
-- their schema, as PostgreSQL's getObjectDescription does.
CREATE ROLE sd_owner;
CREATE ROLE sd_grantee;
CREATE SCHEMA sd_schema AUTHORIZATION sd_owner;
CREATE TABLE sd_t(a integer, b text);
ALTER TABLE sd_t OWNER TO sd_owner;
CREATE TABLE sd_schema.sd_hidden(a integer);
CREATE TYPE sd_type AS (a integer);
ALTER TYPE sd_type OWNER TO sd_owner;
CREATE FUNCTION sd_f(integer) RETURNS integer LANGUAGE sql AS 'SELECT 1';
ALTER FUNCTION sd_f(integer) OWNER TO sd_owner;
CREATE FUNCTION sd_schema.sd_g(sd_type) RETURNS integer LANGUAGE sql AS 'SELECT 1';
ALTER FUNCTION lower(text) OWNER TO sd_owner;
GRANT SELECT ON sd_t TO sd_grantee;
GRANT UPDATE (b) ON sd_t TO sd_grantee;
GRANT USAGE ON SCHEMA sd_schema TO sd_grantee;
GRANT EXECUTE ON FUNCTION upper(text) TO sd_grantee;
GRANT EXECUTE ON FUNCTION sd_f(integer) TO sd_grantee;
CREATE SEQUENCE sd_seq;
GRANT USAGE ON SEQUENCE sd_seq TO sd_grantee;
DROP ROLE sd_owner;
DROP ROLE sd_grantee;
SELECT pg_describe_object(classid, objid, objsubid) AS object, deptype,
       refobjid::regrole AS role
  FROM pg_shdepend
 WHERE refobjid IN ('sd_owner'::regrole, 'sd_grantee'::regrole)
 ORDER BY 1, 2;
SELECT count(*) FROM pg_shdepend WHERE refobjid = 'postgres'::regrole;
SELECT pg_describe_object('pg_class'::regclass, 'sd_t'::regclass, 0),
       pg_describe_object('pg_class'::regclass, 'sd_schema.sd_hidden'::regclass, 0),
       pg_describe_object('pg_class'::regclass, 'sd_t'::regclass, 2),
       pg_describe_object('pg_class'::regclass, 'sd_seq'::regclass, 0);
SELECT pg_describe_object('pg_proc'::regclass, 'lower(text)'::regprocedure, 0),
       pg_describe_object('pg_proc'::regclass, 'sd_f(integer)'::regprocedure, 0),
       pg_describe_object('pg_proc'::regclass, 'sd_schema.sd_g(sd_type)'::regprocedure, 0),
       pg_describe_object('pg_type'::regclass, 'sd_type'::regtype, 0),
       pg_describe_object('pg_type'::regclass, 'integer'::regtype, 0);
SET search_path = sd_schema, public;
SELECT pg_describe_object('pg_class'::regclass, 'sd_hidden'::regclass, 0),
       pg_describe_object('pg_proc'::regclass, 'sd_g(sd_type)'::regprocedure, 0);
RESET search_path;
ALTER FUNCTION lower(text) OWNER TO postgres;
DROP TABLE sd_t;
DROP TYPE sd_type CASCADE;
DROP FUNCTION sd_f(integer);
DROP SEQUENCE sd_seq;
DROP SCHEMA sd_schema CASCADE;
DROP ROLE sd_owner;
DROP ROLE sd_grantee;
REVOKE EXECUTE ON FUNCTION upper(text) FROM sd_grantee;
DROP ROLE sd_grantee;
