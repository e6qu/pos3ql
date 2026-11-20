-- autovacuum_enabled is a table storage parameter: pg_class.reloptions keeps
-- its value as written and the order the options were set in; a SET moves
-- the options it names after the others. Views reject it.
CREATE TABLE r927a (a int) WITH (autovacuum_enabled = off, fillfactor = 70);
CREATE TABLE r927b (a int) WITH (autovacuum_enabled);
CREATE TABLE r927c (a int) WITH (fillfactor = 80, AutoVacuum_Enabled = 'TRUE');
SELECT relname, reloptions FROM pg_class WHERE relname LIKE 'r927%' ORDER BY 1;
ALTER TABLE r927a SET (autovacuum_enabled = true);
ALTER TABLE r927b RESET (autovacuum_enabled);
ALTER TABLE r927c SET (fillfactor = 50);
SELECT relname, reloptions FROM pg_class WHERE relname LIKE 'r927%' ORDER BY 1;
ALTER TABLE r927c SET (autovacuum_enabled = 0, fillfactor = 60);
ALTER TABLE r927b SET (autovacuum_enabled = no);
SELECT relname, reloptions FROM pg_class WHERE relname LIKE 'r927%' ORDER BY 1;
ALTER TABLE r927c RESET (fillfactor);
SELECT relname, reloptions FROM pg_class WHERE relname LIKE 'r927%' ORDER BY 1;
CREATE TABLE r927d (a int) WITH (autovacuum_enabled = maybe);
CREATE TABLE r927d (a int) WITH (autovacuum_enabled = 1, autovacuum_enabled = 0);
CREATE TABLE r927d (a int) WITH (autovacuum_enabled = 2);
CREATE MATERIALIZED VIEW r927m WITH (autovacuum_enabled = false) AS SELECT 1 AS x;
SELECT relname, reloptions FROM pg_class WHERE relname = 'r927m';
CREATE VIEW r927v WITH (autovacuum_enabled = false) AS SELECT 1 AS x;
CREATE VIEW r927v AS SELECT 1 AS x;
ALTER VIEW r927v SET (autovacuum_enabled = false);
ALTER TABLE r927v SET (autovacuum_enabled = false);
DROP VIEW r927v;
DROP MATERIALIZED VIEW r927m;
DROP TABLE r927a, r927b, r927c;
