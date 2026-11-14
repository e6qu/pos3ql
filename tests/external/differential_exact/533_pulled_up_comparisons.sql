-- PostgreSQL 18 pulls a correlated IN or op ANY subquery up into a semi
-- join, equates the joined relation's column to a constant its query's
-- column equals, and scans the inner relation directly for one outer row.
-- A compared subquery left in place prints as its comparison with the
-- subplan's column: hashed for IN, from a materialized copy otherwise.
CREATE TABLE o533 (id int PRIMARY KEY, v int, y text);
CREATE TABLE i533 (id int, oid int, z text);
CREATE TABLE l533 (id int, oid int);
CREATE INDEX l533_oid ON l533 (oid);
INSERT INTO l533 SELECT g, g % 5000 FROM generate_series(1, 6000) g;
ANALYZE l533;
INSERT INTO o533 SELECT g, g % 5, 't' || (g % 3) FROM generate_series(1, 300) g;
INSERT INTO i533 SELECT g, g * 2, 't' || (g % 4) FROM generate_series(1, 200) g;
ANALYZE o533;
ANALYZE i533;
EXPLAIN (COSTS OFF) SELECT id FROM o533 WHERE v IN (SELECT oid FROM i533 WHERE i533.z = o533.y);
SELECT count(*) FROM o533 WHERE v IN (SELECT oid FROM i533 WHERE i533.z = o533.y);
EXPLAIN (COSTS OFF) SELECT id FROM o533 WHERE id = ANY (SELECT oid FROM i533 WHERE i533.id < 10);
SELECT id FROM o533 WHERE id = ANY (SELECT oid FROM i533 WHERE i533.id < 10) ORDER BY id;
EXPLAIN (COSTS OFF) SELECT id FROM o533 WHERE id = 30 AND EXISTS (SELECT 1 FROM i533 WHERE i533.oid = o533.id);
SELECT id FROM o533 WHERE id = 30 AND EXISTS (SELECT 1 FROM i533 WHERE i533.oid = o533.id);
EXPLAIN (COSTS OFF) SELECT id FROM o533 WHERE id = 31 AND NOT EXISTS (SELECT 1 FROM i533 WHERE i533.oid = o533.id);
SELECT id FROM o533 WHERE id = 31 AND NOT EXISTS (SELECT 1 FROM i533 WHERE i533.oid = o533.id);
EXPLAIN (COSTS OFF) SELECT id FROM o533 WHERE id = 7 AND EXISTS (SELECT 1 FROM i533 WHERE i533.id > o533.v);
SELECT id FROM o533 WHERE id = 7 AND EXISTS (SELECT 1 FROM i533 WHERE i533.id > o533.v);
EXPLAIN (COSTS OFF) SELECT id FROM o533 WHERE id IN (SELECT oid FROM i533) OR v = 1;
SELECT count(*) FROM o533 WHERE id IN (SELECT oid FROM i533) OR v = 1;
EXPLAIN (COSTS OFF) SELECT id FROM o533 WHERE id NOT IN (SELECT oid FROM i533);
SELECT count(*) FROM o533 WHERE id NOT IN (SELECT oid FROM i533);
EXPLAIN (COSTS OFF) SELECT id FROM o533 WHERE id <> ALL (SELECT oid FROM i533);
EXPLAIN (COSTS OFF) SELECT id FROM o533 WHERE id > ANY (SELECT oid FROM i533) OR v = 2;
SELECT count(*) FROM o533 WHERE id > ANY (SELECT oid FROM i533) OR v = 2;
EXPLAIN (COSTS OFF) SELECT id FROM o533 WHERE id < ALL (SELECT oid FROM i533 WHERE i533.id > 3);
SELECT id FROM o533 WHERE id < ALL (SELECT oid FROM i533 WHERE i533.id > 3) ORDER BY id;
EXPLAIN (COSTS OFF) SELECT id FROM o533 WHERE v > ANY (SELECT oid FROM i533 WHERE i533.z = o533.y);
SELECT count(*) FROM o533 WHERE v > ANY (SELECT oid FROM i533 WHERE i533.z = o533.y);
-- Constants the query equates its columns to key the joined relation's
-- index probe; results only, as PostgreSQL reads it through a bitmap.
SELECT id FROM o533 WHERE id = 30 AND EXISTS (SELECT 1 FROM l533 WHERE l533.oid = o533.id);
SELECT id FROM o533 WHERE id = 31 AND NOT EXISTS (SELECT 1 FROM l533 WHERE l533.oid = o533.id);
SELECT id FROM o533 WHERE id = 5000 AND NOT EXISTS (SELECT 1 FROM l533 WHERE l533.oid = o533.id);
SELECT count(*) FROM o533 WHERE id < 3 AND EXISTS (SELECT 1 FROM l533 WHERE l533.oid = o533.id);
-- NULLs in a compared subquery: NOT IN and <> ALL find no row.
INSERT INTO i533 VALUES (999, NULL, 'n');
SELECT count(*) FROM o533 WHERE id NOT IN (SELECT oid FROM i533);
SELECT count(*) FROM o533 WHERE id <> ALL (SELECT oid FROM i533);
SELECT count(*) FROM o533 WHERE v IN (SELECT oid FROM i533 WHERE i533.z = o533.y);
-- A view keeps each comparison as written.
CREATE VIEW v533 AS SELECT id FROM o533 WHERE id <> ALL (SELECT oid FROM l533) AND v = ANY (SELECT oid FROM l533) AND id NOT IN (SELECT 1);
SELECT pg_get_viewdef('v533');
DROP VIEW v533;
DROP TABLE o533, i533, l533;
