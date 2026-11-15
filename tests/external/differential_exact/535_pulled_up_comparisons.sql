-- PostgreSQL 18 pulls a correlated IN or op ANY subquery up into a semi
-- join, equates the joined relation's column to a constant its query's
-- column equals, and scans the inner relation directly for one outer row.
-- A compared subquery left in place prints as its comparison with the
-- subplan's column: hashed for IN, from a materialized copy otherwise.
CREATE TABLE o535 (id int PRIMARY KEY, v int, y text);
CREATE TABLE i535 (id int, oid int, z text);
CREATE TABLE l535 (id int, oid int);
CREATE INDEX l535_oid ON l535 (oid);
INSERT INTO l535 SELECT g, g % 5000 FROM generate_series(1, 6000) g;
ANALYZE l535;
INSERT INTO o535 SELECT g, g % 5, 't' || (g % 3) FROM generate_series(1, 300) g;
INSERT INTO i535 SELECT g, g * 2, 't' || (g % 4) FROM generate_series(1, 200) g;
ANALYZE o535;
ANALYZE i535;
EXPLAIN (COSTS OFF) SELECT id FROM o535 WHERE v IN (SELECT oid FROM i535 WHERE i535.z = o535.y);
SELECT count(*) FROM o535 WHERE v IN (SELECT oid FROM i535 WHERE i535.z = o535.y);
EXPLAIN (COSTS OFF) SELECT id FROM o535 WHERE id = ANY (SELECT oid FROM i535 WHERE i535.id < 10);
SELECT id FROM o535 WHERE id = ANY (SELECT oid FROM i535 WHERE i535.id < 10) ORDER BY id;
EXPLAIN (COSTS OFF) SELECT id FROM o535 WHERE id = 30 AND EXISTS (SELECT 1 FROM i535 WHERE i535.oid = o535.id);
SELECT id FROM o535 WHERE id = 30 AND EXISTS (SELECT 1 FROM i535 WHERE i535.oid = o535.id);
EXPLAIN (COSTS OFF) SELECT id FROM o535 WHERE id = 31 AND NOT EXISTS (SELECT 1 FROM i535 WHERE i535.oid = o535.id);
SELECT id FROM o535 WHERE id = 31 AND NOT EXISTS (SELECT 1 FROM i535 WHERE i535.oid = o535.id);
EXPLAIN (COSTS OFF) SELECT id FROM o535 WHERE id = 7 AND EXISTS (SELECT 1 FROM i535 WHERE i535.id > o535.v);
SELECT id FROM o535 WHERE id = 7 AND EXISTS (SELECT 1 FROM i535 WHERE i535.id > o535.v);
EXPLAIN (COSTS OFF) SELECT id FROM o535 WHERE id IN (SELECT oid FROM i535) OR v = 1;
SELECT count(*) FROM o535 WHERE id IN (SELECT oid FROM i535) OR v = 1;
EXPLAIN (COSTS OFF) SELECT id FROM o535 WHERE id NOT IN (SELECT oid FROM i535);
SELECT count(*) FROM o535 WHERE id NOT IN (SELECT oid FROM i535);
EXPLAIN (COSTS OFF) SELECT id FROM o535 WHERE id <> ALL (SELECT oid FROM i535);
EXPLAIN (COSTS OFF) SELECT id FROM o535 WHERE id > ANY (SELECT oid FROM i535) OR v = 2;
SELECT count(*) FROM o535 WHERE id > ANY (SELECT oid FROM i535) OR v = 2;
EXPLAIN (COSTS OFF) SELECT id FROM o535 WHERE id < ALL (SELECT oid FROM i535 WHERE i535.id > 3);
SELECT id FROM o535 WHERE id < ALL (SELECT oid FROM i535 WHERE i535.id > 3) ORDER BY id;
EXPLAIN (COSTS OFF) SELECT id FROM o535 WHERE v > ANY (SELECT oid FROM i535 WHERE i535.z = o535.y);
SELECT count(*) FROM o535 WHERE v > ANY (SELECT oid FROM i535 WHERE i535.z = o535.y);
-- Constants the query equates its columns to key the joined relation's
-- index probe; results only, as PostgreSQL reads it through a bitmap.
SELECT id FROM o535 WHERE id = 30 AND EXISTS (SELECT 1 FROM l535 WHERE l535.oid = o535.id);
SELECT id FROM o535 WHERE id = 31 AND NOT EXISTS (SELECT 1 FROM l535 WHERE l535.oid = o535.id);
SELECT id FROM o535 WHERE id = 5000 AND NOT EXISTS (SELECT 1 FROM l535 WHERE l535.oid = o535.id);
SELECT count(*) FROM o535 WHERE id < 3 AND EXISTS (SELECT 1 FROM l535 WHERE l535.oid = o535.id);
-- NULLs in a compared subquery: NOT IN and <> ALL find no row.
INSERT INTO i535 VALUES (999, NULL, 'n');
SELECT count(*) FROM o535 WHERE id NOT IN (SELECT oid FROM i535);
SELECT count(*) FROM o535 WHERE id <> ALL (SELECT oid FROM i535);
SELECT count(*) FROM o535 WHERE v IN (SELECT oid FROM i535 WHERE i535.z = o535.y);
-- A view keeps each comparison as written.
CREATE VIEW v535 AS SELECT id FROM o535 WHERE id <> ALL (SELECT oid FROM l535) AND v = ANY (SELECT oid FROM l535) AND id NOT IN (SELECT 1);
SELECT pg_get_viewdef('v535');
DROP VIEW v535;
DROP TABLE o535, i535, l535;
