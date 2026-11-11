-- pg_type has PostgreSQL's 32 columns, every pg_catalog type exactly, and the
-- columns PostgreSQL derives for user-defined domains, enums, composites,
-- relation row types and their arrays.
SELECT * FROM pg_type WHERE typnamespace = 'pg_catalog'::regnamespace ORDER BY oid;
CREATE SCHEMA tcr;
SET search_path = tcr;
CREATE DOMAIN dm AS int NOT NULL DEFAULT 3;
CREATE DOMAIN dv AS varchar(5);
CREATE TYPE en AS ENUM ('a');
CREATE DOMAIN da AS en[];
CREATE DOMAIN dd AS dm;
CREATE TYPE ct AS (x int);
CREATE TABLE t (a int);
CREATE VIEW v AS SELECT 1 AS x;
SELECT typname, typlen, typbyval, typtype, typcategory, typispreferred, typisdefined,
       typdelim, typrelid <> 0, typsubscript, typelem::regtype, typarray::regtype,
       typinput, typoutput, typreceive, typsend, typmodin, typmodout, typanalyze,
       typalign, typstorage, typnotnull, typbasetype::regtype, typtypmod, typndims,
       typcollation, typdefault
FROM pg_type WHERE typnamespace = 'tcr'::regnamespace ORDER BY typname;
RESET search_path;
DROP SCHEMA tcr CASCADE;
