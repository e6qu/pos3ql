-- PostgreSQL 17+ evaluates the expressions of CREATE INDEX, REINDEX,
-- ANALYZE, ALTER TABLE's index builds and materialized view population
-- under search_path = pg_catalog, pg_temp (RestrictSearchPath), after the
-- command's own names are resolved: a domain outside that path is named
-- qualified in a diagnostic, and the evaluation reports the restricted
-- setting and schemas.
\set VERBOSITY default
CREATE SCHEMA s754;
CREATE DOMAIN posint754 AS int CHECK (VALUE > 0);
CREATE DOMAIN s754.small754 AS int CHECK (VALUE < 100);
CREATE TABLE t754 (a int, b int);
INSERT INTO t754 VALUES (1, 5), (-1, 500);
CREATE INDEX t754_dom ON t754 ((a::posint754));
CREATE INDEX t754_part ON t754 (a) WHERE a::posint754 > 0;
CREATE UNIQUE INDEX t754_uniq ON t754 ((b::s754.small754));
ALTER TABLE t754 ADD CONSTRAINT t754_excl EXCLUDE ((a::posint754) WITH =);
CREATE MATERIALIZED VIEW m754 AS SELECT a::posint754 AS p FROM t754;
CREATE MATERIALIZED VIEW m754_empty AS SELECT a::posint754 AS p FROM t754 WITH NO DATA;
REFRESH MATERIALIZED VIEW m754_empty;
-- An ordinary statement keeps the session's path.
SELECT a::posint754 FROM t754;
INSERT INTO t754 VALUES (-2, 1) RETURNING a::posint754;
DELETE FROM t754 WHERE a < 0;
CREATE INDEX t754_dom ON t754 ((a::posint754));
REFRESH MATERIALIZED VIEW m754_empty;
UPDATE t754 SET a = -3;
REINDEX INDEX t754_dom;
-- A retyped column rebuilds its indexes under the restricted path, after
-- the rewrite itself evaluated the conversion under the session's path.
CREATE INDEX t754_b ON t754 ((b::posint754));
ALTER TABLE t754 ALTER COLUMN b TYPE bigint USING (b - 1000)::posint754;
ALTER TABLE t754 ALTER COLUMN b TYPE bigint USING b - 1000;
ALTER TABLE t754 ADD PRIMARY KEY (a);
-- The populating query reports the restricted setting and schemas.
CREATE MATERIALIZED VIEW m754_path AS
  SELECT current_setting('search_path') AS path, current_schema() AS schema,
         current_schemas(true) AS schemas;
SELECT * FROM m754_path;
SET search_path = s754, public;
REFRESH MATERIALIZED VIEW m754_path;
SELECT * FROM m754_path;
SELECT current_setting('search_path'), current_schema();
RESET search_path;
