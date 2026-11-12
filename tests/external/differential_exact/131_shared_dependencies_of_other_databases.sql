-- pg_shdepend holds the rows of objects in every database.
\set QUIET on
CREATE ROLE sd_owner;
CREATE ROLE sd_reader;
CREATE DATABASE sd_other;
\c sd_other
CREATE TABLE sd_t(a integer);
ALTER TABLE sd_t OWNER TO sd_owner;
GRANT SELECT ON sd_t TO sd_reader;
CREATE SCHEMA sd_s AUTHORIZATION sd_owner;
\c postgres
\set QUIET off
SELECT d.datname, s.classid::regclass, s.objsubid, s.refobjid::regrole, s.deptype FROM pg_shdepend s JOIN pg_database d ON d.oid = s.dbid WHERE s.refobjid IN ('sd_owner'::regrole, 'sd_reader'::regrole) ORDER BY 1, 2, 4, 5;

\set QUIET on
DROP DATABASE sd_other;
DROP ROLE sd_owner, sd_reader;
