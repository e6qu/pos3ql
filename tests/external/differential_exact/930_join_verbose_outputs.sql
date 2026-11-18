-- EXPLAIN VERBOSE of plans PostgreSQL's join search builds prints each
-- node's target list as createplan.c builds it: the top join computes the
-- select list, a lower join its relations' columns needed above it, a
-- scan under a Hash, Sort, Materialize or Memoize only the columns needed,
-- another scan its physical columns (an index-only scan its index's), and
-- a unique-ified relation or a merge sort the expressions it compares.
CREATE TABLE va930 (k int, w int, t text);
CREATE TABLE vb930 (k int, w int, u text);
CREATE TABLE vc930 (k int, v int, z int);
INSERT INTO va930 SELECT g, g % 13, 'a' || g FROM generate_series(1, 900) g;
INSERT INTO vb930 SELECT g, g % 7, 'b' || g FROM generate_series(1, 300) g;
INSERT INTO vc930 SELECT g % 300, g, g % 5 FROM generate_series(1, 1200) g;
ALTER TABLE vb930 ADD PRIMARY KEY (k);
CREATE INDEX vc930_k ON vc930 (k);
CREATE INDEX vb930_wk ON vb930 (w, k);
ANALYZE va930, vb930, vc930;
EXPLAIN (VERBOSE, COSTS OFF) SELECT va930.t, vb930.u FROM va930 JOIN vb930 ON va930.k = vb930.k;
EXPLAIN (VERBOSE, COSTS OFF) SELECT va930.t FROM va930 JOIN vb930 ON va930.k = vb930.k JOIN vc930 ON vc930.k = vb930.k WHERE vc930.z = 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT va930.w, vb930.u FROM va930 JOIN vb930 ON va930.k = vb930.k ORDER BY va930.w LIMIT 5;
EXPLAIN (VERBOSE, COSTS OFF) SELECT va930.k + vb930.w AS s FROM va930 LEFT JOIN vb930 ON va930.k = vb930.k WHERE va930.w = 3;
EXPLAIN (VERBOSE) SELECT va930.k + vb930.w AS s FROM va930 LEFT JOIN vb930 ON va930.k = vb930.k WHERE va930.w = 3;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM va930 WHERE k IN (SELECT k + 1 FROM vc930 WHERE z = 2);
EXPLAIN (VERBOSE, COSTS OFF) SELECT va930.t FROM va930 WHERE NOT EXISTS (SELECT 1 FROM vb930 WHERE vb930.k = va930.k);
EXPLAIN (VERBOSE, COSTS OFF) SELECT vb930.k, vc930.v FROM vb930 JOIN vc930 ON vb930.k = vc930.k WHERE vb930.w = 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT vb930.w, vc930.v FROM vb930 JOIN vc930 ON vb930.k = vc930.k WHERE vb930.w = 2 AND vb930.k < 50;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM va930 WHERE k IN (SELECT k FROM vc930 WHERE z = 2);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM vb930 WHERE k IN (SELECT k + 1 FROM vc930 WHERE z = 2);
EXPLAIN (VERBOSE, COSTS OFF) SELECT vb930.u FROM va930 JOIN vb930 ON va930.k * 2 = vb930.k ORDER BY va930.k * 2 LIMIT 3;
SET enable_hashjoin = off;
EXPLAIN (VERBOSE, COSTS OFF) SELECT va930.t, vb930.u FROM va930 JOIN vb930 ON va930.k = vb930.k;
EXPLAIN (VERBOSE, COSTS OFF) SELECT va930.t FROM va930 JOIN vb930 ON va930.k + 1 = vb930.k;
SET enable_mergejoin = off;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM vb930 WHERE k IN (SELECT k FROM vc930 WHERE v < 10);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM vb930 WHERE k IN (SELECT k + 1 FROM vc930 WHERE v < 10);
SET enable_hashagg = off;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM vb930 WHERE k IN (SELECT k FROM vc930 WHERE v < 10);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM vb930 WHERE k IN (SELECT k + 1 FROM vc930 WHERE v < 10);
SELECT count(*), sum(k) FROM vb930 WHERE k IN (SELECT k + 1 FROM vc930 WHERE v < 10);
RESET enable_hashagg;
RESET enable_mergejoin;
RESET enable_hashjoin;
SET work_mem = '64kB';
EXPLAIN (VERBOSE, COSTS OFF) SELECT va930.t, vc930.v FROM va930 JOIN vc930 ON va930.k = vc930.v;
RESET work_mem;
SELECT count(*), sum(va930.k) FROM va930 JOIN vb930 ON va930.k + 1 = vb930.k;
DROP TABLE va930, vb930, vc930;
