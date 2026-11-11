-- DROP ROLE counts the dependent objects in other databases.
\set origin :DBNAME
\set QUIET on
CREATE ROLE odb_owner;
CREATE ROLE odb_reader;
CREATE DATABASE odb_other;
CREATE DATABASE odb_owned OWNER odb_owner;
\c odb_other
CREATE TABLE odb_t(a integer);
ALTER TABLE odb_t OWNER TO odb_owner;
CREATE TABLE odb_u(a integer);
ALTER TABLE odb_u OWNER TO odb_owner;
GRANT SELECT ON odb_t TO odb_reader;
\c :origin
CREATE TABLE odb_local(a integer);
ALTER TABLE odb_local OWNER TO odb_owner;
DROP ROLE odb_owner;
DROP ROLE odb_reader;
DROP TABLE odb_local;
DROP ROLE odb_owner;
\c odb_other
DROP TABLE odb_t, odb_u;
\c :origin
DROP DATABASE odb_other;
DROP DATABASE odb_owned;
DROP ROLE odb_owner, odb_reader;
