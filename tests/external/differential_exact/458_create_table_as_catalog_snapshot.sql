-- CREATE TABLE AS and SELECT INTO run their query before the new table
-- exists; a materialized view, as in PostgreSQL, sees itself.
CREATE TABLE ct AS SELECT count(*) AS n FROM pg_class WHERE relname = 'ct';
SELECT * FROM ct;
CREATE TABLE ct2 AS SELECT relname FROM pg_class WHERE relname LIKE 'ct%' ORDER BY 1;
SELECT * FROM ct2;
SELECT * INTO ct3 FROM pg_class WHERE relname = 'ct3';
SELECT count(*) FROM ct3;
CREATE MATERIALIZED VIEW ctm AS SELECT count(*) AS n FROM pg_class WHERE relname = 'ctm';
SELECT * FROM ctm;
REFRESH MATERIALIZED VIEW ctm;
SELECT * FROM ctm;
CREATE TABLE ct4 AS SELECT attname FROM pg_attribute WHERE attrelid = (SELECT oid FROM pg_class WHERE relname = 'ct4');
SELECT count(*) FROM ct4;
CREATE TABLE ct_src (x int);
INSERT INTO ct_src VALUES (0);
CREATE TABLE ct AS SELECT 1 / x FROM ct_src;
CREATE TABLE ct AS SELECT 1 / 0;
CREATE TABLE IF NOT EXISTS ct AS SELECT 1 / x FROM ct_src;
CREATE TABLE ct5 AS SELECT 1 / x FROM ct_src;
SELECT count(*) FROM pg_class WHERE relname = 'ct5';
CREATE TABLE ct6 AS SELECT count(*) FROM pg_class WHERE relname = 'ct6' WITH NO DATA;
SELECT count(*) FROM ct6;
