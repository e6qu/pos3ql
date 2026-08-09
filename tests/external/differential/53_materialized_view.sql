-- CREATE / REFRESH / DROP MATERIALIZED VIEW, matching PostgreSQL 18. The rows
-- are stored in a same-named backing table; the defining query is re-run only
-- by REFRESH. (In the forced-spill differential run this also exercises the
-- matview manifest line's checkpoint round-trip.)
DROP TABLE IF EXISTS base;
CREATE TABLE base (id int, v int);
INSERT INTO base VALUES (1, 10), (2, 20), (3, 30);

-- information_schema.columns derives ordinary-view output under the view's
-- captured creation path, just like pg_attribute and Describe.
CREATE VIEW catalog_view AS SELECT id, v::text AS rendered FROM base;
SELECT column_name, ordinal_position, is_nullable, data_type
FROM information_schema.columns
WHERE table_name = 'catalog_view'
ORDER BY ordinal_position;
DROP VIEW catalog_view;

-- A materialized view captures the query's rows at creation time.
CREATE MATERIALIZED VIEW mv AS SELECT id, v * 2 AS dbl FROM base WHERE v > 15;
SELECT * FROM mv ORDER BY id;

-- Base changes are NOT reflected until REFRESH.
INSERT INTO base VALUES (4, 40);
SELECT * FROM mv ORDER BY id;
REFRESH MATERIALIZED VIEW mv;
SELECT * FROM mv ORDER BY id;

-- It is a materialized view in the catalog (relkind 'm'), and pg_matviews
-- lists it.
SELECT relkind FROM pg_class WHERE relname = 'mv';
SELECT matviewname, ispopulated FROM pg_matviews WHERE matviewname = 'mv';

-- WITH NO DATA creates it unpopulated; REFRESH fills it.
CREATE MATERIALIZED VIEW mv2 AS SELECT id FROM base WITH NO DATA;
SELECT ispopulated FROM pg_matviews WHERE matviewname = 'mv2';
REFRESH MATERIALIZED VIEW mv2;
SELECT count(*) FROM mv2;

-- A materialized view over a set-operation body.
CREATE MATERIALIZED VIEW mv3 AS SELECT id FROM base WHERE id = 1 UNION SELECT id FROM base WHERE id = 4;
SELECT id FROM mv3 ORDER BY id;

-- DROP TABLE is refused on a materialized view (42809); DROP MATERIALIZED VIEW
-- removes it. DROP MATERIALIZED VIEW on a plain table is refused too.
DROP TABLE mv;
DROP MATERIALIZED VIEW base;
DROP MATERIALIZED VIEW mv;
DROP MATERIALIZED VIEW mv2;
DROP MATERIALIZED VIEW mv3;
DROP TABLE base;

-- IF NOT EXISTS / IF EXISTS.
CREATE MATERIALIZED VIEW IF NOT EXISTS mvx AS SELECT 1 AS a;
CREATE MATERIALIZED VIEW IF NOT EXISTS mvx AS SELECT 2 AS a;
SELECT a FROM mvx;
DROP MATERIALIZED VIEW IF EXISTS mvx;
DROP MATERIALIZED VIEW IF EXISTS mvx;
